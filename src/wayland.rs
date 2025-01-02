/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! Types and basic parsing/writing code for Wayland protocol */
use crate::wayland_gen::{WaylandInterface, INTERFACE_TABLE};

pub const PARSE_ERROR: &str = "Failed to parse message";

/* Wayland object Id. The null object 0 is not special cased and may require checking. */
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone, Copy)]
#[repr(transparent)]
pub struct ObjId(pub u32);
impl std::fmt::Display for ObjId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub enum WaylandArgument {
    Int,
    Uint,
    Fixed,
    String,
    OptionalString,
    Object(WaylandInterface),
    GenericObject,
    NewId(WaylandInterface),
    GenericNewId,
    Array,
    Fd,
}
pub struct WaylandMethod {
    pub name: &'static str,
    pub sig: &'static [WaylandArgument],
    pub destructor: bool,
}
// TODO: memory layout optimizations. As with the C implementation, could
// compact this data a lot by merging all these arrays into a single table,
// and using indices into it
// In particular: unify the event/req tables, by making them use an index
// into a global table, placing events at coordinates x+0,x+1,x+2..., and placing
// requests at coordinates x-1,x-2,x-3...

/** Data for a Wayland interface. */
pub struct WaylandData {
    pub name: &'static str,
    pub evts: &'static [WaylandMethod],
    pub reqs: &'static [WaylandMethod],
    pub version: u32,
}

#[derive(PartialEq, Eq, Copy, Clone)]
pub enum MethodId {
    Event(u8),
    Request(u8),
}
impl MethodId {
    pub fn code(&self) -> u8 {
        match self {
            Self::Event(ref x) => *x,
            Self::Request(ref x) => *x,
        }
    }
}
impl std::fmt::Display for MethodId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Event(ref x) => {
                write!(f, "e{}", x)
            }
            Self::Request(ref x) => {
                write!(f, "r{}", x)
            }
        }
    }
}

pub fn lookup_intf_by_name(name: &[u8]) -> Option<WaylandInterface> {
    let r = INTERFACE_TABLE.binary_search_by(|cand_intf| cand_intf.name.as_bytes().cmp(name));
    if let Ok(idx) = r {
        Some(WaylandInterface::try_from(idx as u32).unwrap())
    } else {
        None
    }
}
pub fn parse_wl_header(msg: &[u8]) -> (ObjId, usize, u8) {
    let obj_id = ObjId(u32::from_le_bytes(msg[0..4].try_into().unwrap()));
    let x = u32::from_le_bytes(msg[4..8].try_into().unwrap());
    let length = (x >> 16) as usize;
    let opcode = (x & 0x00ff) as u8;
    (obj_id, length, opcode)
}
pub fn parse_string<'a>(msg_tail: &mut &'a [u8]) -> Result<Option<&'a [u8]>, &'static str> {
    let v = parse_array(msg_tail)?;
    // Null values for Wayland strings are encoded with empty arrays
    if v.is_empty() {
        Ok(None)
    } else {
        // drop null terminator
        Ok(Some(&v[..v.len() - 1]))
    }
}
pub fn parse_array<'a>(msg_tail: &mut &'a [u8]) -> Result<&'a [u8], &'static str> {
    if msg_tail.len() < 4 {
        return Err(PARSE_ERROR);
    }
    let length = u32::from_le_bytes(msg_tail[..4].try_into().unwrap()) as usize;
    // length includes null terminator, if string
    let space = length.checked_next_multiple_of(4).ok_or(PARSE_ERROR)?;
    if space > msg_tail.len() - 4 {
        return Err(PARSE_ERROR);
    }
    let ret = &msg_tail[4..4 + length];
    *msg_tail = &std::mem::take(msg_tail)[4 + space..];
    Ok(ret)
}
pub fn parse_obj(msg_tail: &mut &[u8]) -> Result<ObjId, &'static str> {
    Ok(ObjId(parse_u32(msg_tail)?))
}
pub fn parse_u32(msg_tail: &mut &[u8]) -> Result<u32, &'static str> {
    if msg_tail.len() < 4 {
        return Err(PARSE_ERROR);
    }
    let val = u32::from_le_bytes(msg_tail[0..4].try_into().unwrap());
    *msg_tail = &std::mem::take(msg_tail)[4..];
    Ok(val)
}
pub fn parse_i32(msg_tail: &mut &[u8]) -> Result<i32, &'static str> {
    if msg_tail.len() < 4 {
        return Err(PARSE_ERROR);
    }
    let val = i32::from_le_bytes(msg_tail[0..4].try_into().unwrap());
    *msg_tail = &std::mem::take(msg_tail)[4..];
    Ok(val)
}
pub fn length_array(arr_len: usize) -> usize {
    let arrlen = arr_len;
    arrlen
        .checked_next_multiple_of(4)
        .unwrap_or(usize::MAX)
        .saturating_add(4)
}
pub fn length_string(str_len_unterminated: usize) -> usize {
    length_array(str_len_unterminated.saturating_add(1))
}
pub fn write_header(
    tail: &mut &mut [u8],
    obj_id: ObjId,
    length: usize,
    opcode: usize,
    ntagfds: u32,
) {
    let id = u32::to_le_bytes(obj_id.0);
    assert!(length < (1 << 16) && ntagfds < (1 << 5) && opcode < (1 << 8));
    let p = u32::to_le_bytes((length as u32) << 16 | (ntagfds << 11) | (opcode as u32) & 0x00ff);
    tail[..4].copy_from_slice(&id);
    tail[4..8].copy_from_slice(&p);
    *tail = &mut std::mem::take(tail)[8..];
}
pub fn write_u32(tail: &mut &mut [u8], val: u32) {
    let v = u32::to_le_bytes(val);
    tail[..4].copy_from_slice(&v);
    *tail = &mut std::mem::take(tail)[4..];
}
pub fn write_obj(tail: &mut &mut [u8], val: ObjId) {
    write_u32(tail, val.0)
}
pub fn write_i32(tail: &mut &mut [u8], val: i32) {
    let v = i32::to_le_bytes(val);
    tail[..4].copy_from_slice(&v);
    *tail = &mut std::mem::take(tail)[4..];
}
pub fn write_array(tail: &mut &mut [u8], s: &[u8]) {
    let l = u32::to_le_bytes(s.len() as u32);
    tail[..4].copy_from_slice(&l);
    tail[4..(s.len() + 4)].copy_from_slice(s);
    let e = length_array(s.len());
    tail[(s.len() + 4)..e].fill(0);
    *tail = &mut std::mem::take(tail)[e..];
}
pub fn write_string(tail: &mut &mut [u8], os: Option<&[u8]>) {
    if let Some(s) = os {
        let l = u32::to_le_bytes(s.len() as u32 + 1);
        tail[..4].copy_from_slice(&l);
        tail[4..(s.len() + 4)].copy_from_slice(s);
        let e = length_string(s.len());
        tail[(s.len() + 4)..e].fill(0);
        *tail = &mut std::mem::take(tail)[e..];
    } else {
        tail[0..4].fill(0);
        *tail = &mut std::mem::take(tail)[4..];
    }
}
