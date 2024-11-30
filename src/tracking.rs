/* SPDX-License-Identifier: GPL-3.0-or-later */

use crate::damage::*;
#[cfg(feature = "dmabuf")]
use crate::dmabuf::*;
use crate::kernel::*;
use crate::mainloop::*;
use crate::mirror::*;
#[cfg(not(feature = "video"))]
use crate::stub::*;
use crate::tag;
use crate::util::*;
use crate::wayland::*;
use crate::wayland_gen::*;

use log::{debug, error};
use nix::libc;
use nix::sys::memfd;
use nix::unistd;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;

pub struct WpObject {
    obj_type: WaylandInterface,
    is_zombie: bool,
    extra: WpExtra,
}
#[derive(Clone, Copy, Debug)]
struct WlRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}
#[derive(Clone)]
struct DamageBatch {
    /* wl_surface.damage is interpreted to have the scale/transform at the time the damage was committed,
     * so these must be recorded per commit. (It is unclear how this interacts with fractional scale and
     * wp_viewporter.) (The buffer dimensions must be known in order to apply the scale and transform,
     * so we delay this to better handle scenarios where the surface alternates between differently
     * sized buffers.)
     *
     * Most clients use the better wl_buffer.damage_buffer, so perfection here is not critical. */
    scale: u32,
    transform: WlOutputTransform,
    damage: Vec<WlRect>,
    damage_buffer: Vec<WlRect>,

    buffer_uid: u64,
}
struct ObjWlSurface {
    attached_buffer_id: Option<ObjId>,
    /* The total damage for a buffer since the last time it was committed is given
     * by the accumulateed damage committed. The pending state is at index 0,
     * last commit at index 1, etc. */
    damage_history: [DamageBatch; 7],
    /* acquire/release timline points for explicit sync */
    acquire_pt: Option<(u64, Rc<RefCell<ShadowFd>>)>,
    release_pt: Option<(u64, Rc<RefCell<ShadowFd>>)>,
}
struct ObjWlShmPool {
    buffer: Rc<RefCell<ShadowFd>>,
}

struct ObjWlBufferShm {
    width: i32,
    height: i32,
    format: u32,
    offset: i32,
    stride: i32,
}

struct ObjWlBuffer {
    sfd: Rc<RefCell<ShadowFd>>,
    /* Metadata explaining how a wl_buffer relates to its underlying wl_shm_pool.'
     * DMABUFs do not need this metadata because the shadowfd stores this information. */
    shm_info: Option<ObjWlBufferShm>,

    unique_id: u64,
}
struct DmabufTranche {
    flags: u32,
    /* note: these can only be interpreted when the "done" event arrives
     * and the format table is known */
    entries: Vec<u16>,
    device: u64,
}

struct ObjZwpLinuxDmabuf {
    /* Set of formats seen in .modifier events. This makes it possible to
     * replace the first .modifier received with a full list of modifiers events
     * for that format, and then drop all subsequent .modifier events with same
     * format. (Alternatively, one could wait for "a roundtrip after binding" to
     * determine when all format events have arrived, but this approach will never
     * introduce any delay. */
    formats_seen: BTreeSet<u32>,
}

struct ObjZwpLinuxDmabufFeedback {
    /* Cache of the last sent table fd */
    table_fd: Option<(OwnedFd, u32)>,
    table_sfd: Option<Rc<RefCell<ShadowFd>>>,
    format_table: Vec<(u32, u64)>,

    main_device: Option<u64>,
    tranches: Vec<DmabufTranche>,
    current: DmabufTranche,
}

struct ObjZwpLinuxDmabufParams {
    /* if using 'create', output dmabuf reference stored here before transferring to wl_buffer */
    dmabuf: Option<Rc<RefCell<ShadowFd>>>,
    // A bunch of zwp_linux_buffer_params_v1.add calls will be followed by create or create_immed,
    // which provide the dimensions and format
    planes: Vec<AddDmabufPlane>,
    // todo: the size is limited in practice, so instead of Vec one could use an array;
    // however, this would require some way of handling drop (e.g., using Option<OwnedFd>)
    // which might be awkward
}

struct ObjWpDrmSyncobjSurface {
    /* Corresponding wl_surface object */
    surface: ObjId,
}

struct ObjWpDrmSyncobjTimeline {
    timeline: Rc<RefCell<ShadowFd>>,
}

enum ClockOrObjects {
    Clock(u32),
    Objects(Vec<ObjId>),
}

struct ObjWpPresentation {
    /* Either the clock id, once set, or the list of feedback objects which are waiting to receive
     * a clock id */
    state: ClockOrObjects,
}

#[derive(Debug)]
enum ClockOrObject {
    Clock(u32),
    Object(ObjId),
    /* wp_presentation deleted before clock identified, cannot use */
    Invalid,
}

struct ObjWpPresentationFeedback {
    /* It is possible to create a wp_presentation_feedback object before the event setting
     * the clock id onn wp_presentation is received; */
    clock: ClockOrObject,
}

// issue: enum size overhead. Consider using Box<ObjWlSurface>, Box<ObjWlBuffer>, etc. entries?
enum WpExtra {
    WlSurface(Box<ObjWlSurface>),
    WlBuffer(Box<ObjWlBuffer>),
    WlShmPool(Box<ObjWlShmPool>),
    ZwpDmabuf(Box<ObjZwpLinuxDmabuf>),
    ZwpDmabufFeedback(Box<ObjZwpLinuxDmabufFeedback>),
    ZwpDmabufParams(Box<ObjZwpLinuxDmabufParams>),
    WpDrmSyncobjSurface(Box<ObjWpDrmSyncobjSurface>),
    WpDrmSyncobjTimeline(Box<ObjWpDrmSyncobjTimeline>),
    WpPresentation(Box<ObjWpPresentation>),
    WpPresentationFeedback(Box<ObjWpPresentationFeedback>),
    None,
}

pub enum TranslationInfo<'a> {
    // RID -> SFD/FD
    FromChannel(
        (
            &'a mut VecDeque<Rc<RefCell<ShadowFd>>>,
            &'a mut VecDeque<Rc<RefCell<ShadowFd>>>,
        ),
    ),
    // FD to SFD/RID
    FromWayland(
        (
            &'a mut VecDeque<OwnedFd>,
            &'a mut Vec<Rc<RefCell<ShadowFd>>>,
        ),
    ),
}

fn clip_wlrect_to_buffer(a: &WlRect, w: i32, h: i32) -> Option<Rect> {
    let x1 = a.x;
    let x2 = a.x.saturating_add(a.width);
    let y1 = a.y;
    let y2 = a.y.saturating_add(a.height);
    let nx1 = x1.clamp(0, w);
    let nx2 = x2.clamp(0, w);
    let ny1 = y1.clamp(0, h);
    let ny2 = y2.clamp(0, h);
    if nx2 > nx1 && ny2 > ny1 {
        Some(Rect {
            x1: nx1 as u32,
            x2: nx2 as u32,
            y1: ny1 as u32,
            y2: ny2 as u32,
        })
    } else {
        None
    }
}

/* This handles out-of-bounds behavior by saturating, instead of erroring, since damage gets
 * clipped later anyway; and damage(0,0,INT_MAX,INT_MAX) is an occasional idiom */
fn apply_damage_rect_transform(
    a: &WlRect,
    scale: u32,
    transform: WlOutputTransform,
    width: i32,
    height: i32,
) -> WlRect {
    let mut xl = a.x.saturating_mul(scale as i32); // will not overflow: scale was originally i32
    let mut yl = a.y.saturating_mul(scale as i32);
    let mut xh = (a.x.saturating_add(a.width)).saturating_mul(scale as i32);
    let mut yh = (a.y.saturating_add(a.height)).saturating_mul(scale as i32);

    /* Each of the eight transformations corresponds to a
     * unique set of reflections: X<->Y | Xflip | Yflip */
    let seq = [0b000, 0b011, 0b110, 0b101, 0b010, 0b001, 0b100, 0b111];
    let code = seq[transform as u32 as usize];
    let swap_xy = code & 0x1 != 0;
    let flip_x = code & 0x2 != 0;
    let flip_y = code & 0x4 != 0;

    let end_w = if swap_xy { height } else { width };
    let end_h = if swap_xy { width } else { height };

    if flip_x {
        (xh, xl) = (end_w - xl, end_w - xh);
    }
    if flip_y {
        (yh, yl) = (end_h - yl, end_h - yh);
    }
    if swap_xy {
        (xl, xh, yl, yh) = (yl, yh, xl, xh);
    }
    WlRect {
        x: xl,
        width: xh.saturating_sub(xl),
        y: yl,
        height: yh.saturating_sub(yl),
    }
}

fn damage_for_entire_buffer(buffer: &ObjWlBufferShm) -> (usize, usize) {
    let start = (buffer.offset) as usize;
    let end = (buffer.offset + buffer.stride * buffer.height) as usize;
    assert!(start < end);
    (64 * (start / 64), align(end, 64))
}

fn get_bpp(fmt: u32) -> Option<usize> {
    use WlShmFormat::*;

    let f: WlShmFormat = fmt.try_into().ok()?;
    match f {
        Argb8888 | Xrgb8888 => Some(4),
        C8 | Rgb332 | Bgr233 => Some(1),
        Xrgb4444 | Xbgr4444 | Rgbx4444 | Bgrx4444 | Argb4444 | Abgr4444 | Rgba4444 | Bgra4444
        | Xrgb1555 | Xbgr1555 | Rgbx5551 | Bgrx5551 | Argb1555 | Abgr1555 | Rgba5551 | Bgra5551
        | Rgb565 | Bgr565 => Some(2),
        Rgb888 | Bgr888 => Some(3),
        Xbgr8888 | Rgbx8888 | Bgrx8888 | Abgr8888 | Rgba8888 | Bgra8888 | Xrgb2101010
        | Xbgr2101010 | Rgbx1010102 | Bgrx1010102 | Argb2101010 | Abgr2101010 | Rgba1010102
        | Bgra1010102 => Some(4),
        Yuyv | Yvyu | Uyvy | Vyuy | Ayuv => Some(4),
        Nv12 | Nv21 | Nv16 | Nv61 | Yuv410 | Yvu410 | Yuv411 | Yvu411 | Yuv420 | Yvu420
        | Yuv422 | Yvu422 | Yuv444 | Yvu444 => None,
        R8 => Some(1),
        R16 | Rg88 | Gr88 => Some(2),
        Rg1616 | Gr1616 => Some(4),
        Xrgb16161616f | Xbgr16161616f | Argb16161616f | Abgr16161616f => Some(8),
        Xyuv8888 => Some(4),
        Vuy888 => Some(3),
        Vuy101010 => None,
        Y210 | Y212 | Y216 => None, // subsampled packed
        Y410 | Y412 | Y416 => None,
        Xvyu2101010 => Some(4),
        Xvyu1216161616 => Some(8),
        Xvyu16161616 => Some(4),
        Y0l0 | X0l0 | Y0l2 | X0l2 => None, // 2x2 tiled
        Yuv4208bit => None,
        Yuv42010bit => None,
        Xrgb8888A8 | Xbgr8888A8 | Rgbx8888A8 | Bgrx8888A8 | Rgb888A8 | Bgr888A8 | Rgb565A8
        | Bgr565A8 | Nv24 | Nv42 | P210 | P010 | P012 | P016 => None,
        Axbxgxrx106106106106 => Some(8),
        Nv15 | Q410 | Q401 => None,
        Xrgb16161616 | Xbgr16161616 | Argb16161616 | Abgr16161616 => Some(8),
        C1 | C2 | C4 | D1 | D2 | D4 => None,
        D8 => Some(1),
        R1 | R2 | R4 => None,
        R10 => Some(2),
        R12 => Some(2),
        Avuy8888 | Xvuy8888 => Some(4),
        P030 => None,
    }
}

fn get_damage_rects(surface: &ObjWlSurface, buffer_uid: u64, width: i32, height: i32) -> Vec<Rect> {
    let mut rects = Vec::<Rect>::new();
    let Some(mut first_idx) = surface
        .damage_history
        .iter()
        .position(|x| x.buffer_uid == buffer_uid)
    else {
        /* First time buffer was seen; mark the entire buffer as damaged */
        rects.push(Rect {
            x1: 0,
            x2: width.try_into().unwrap(),
            y1: 0,
            y2: height.try_into().unwrap(),
        });
        return rects;
    };

    /* Except for the first slot (which gets updated later by this function's caller),
     * if buffer_uid = buffer.unique_id, then the damage was already applied.
     * Also, damage_history[0].buffer_uid = damage_history[1].buffer_uid at this point. */
    first_idx = std::cmp::max(first_idx, 1);

    for batch in &surface.damage_history[..first_idx] {
        for w in &batch.damage_buffer {
            if let Some(r) = clip_wlrect_to_buffer(w, width, height) {
                rects.push(r);
            }
        }

        for w in &batch.damage {
            let q = apply_damage_rect_transform(w, batch.scale, batch.transform, width, height);
            if let Some(r) = clip_wlrect_to_buffer(&q, width, height) {
                rects.push(r);
            }
        }
    }
    rects
}

fn get_damage_for_shm(buffer: &ObjWlBuffer, surface: &ObjWlSurface) -> Vec<(usize, usize)> {
    let Some(shm_info) = &buffer.shm_info else {
        panic!();
    };

    let Some(bpp) = get_bpp(shm_info.format) else {
        debug!("Format without known bpp {}", shm_info.format);
        return vec![damage_for_entire_buffer(shm_info)];
    };

    let mut rects = get_damage_rects(surface, buffer.unique_id, shm_info.width, shm_info.height);
    compute_damaged_segments(
        &mut rects[..],
        6,
        128,
        shm_info.offset.try_into().unwrap(),
        shm_info.stride.try_into().unwrap(),
        bpp,
    )
}

fn get_damage_for_dmabuf(
    buffer: &ObjWlBuffer,
    sfdd: &ShadowFdDmabuf,
    surface: &ObjWlSurface,
) -> Vec<(usize, usize)> {
    let nom_len = sfdd.buf.nominal_size(sfdd.view_row_stride);
    let (width, height) = (sfdd.buf.width, sfdd.buf.height);

    let wayl_format = drm_to_wayland(sfdd.drm_format);
    let Some(bpp) = get_bpp(wayl_format) else {
        debug!("Format without known bpp {}", sfdd.drm_format);
        return vec![(0, align(nom_len, 64))];
    };

    let mut rects = get_damage_rects(surface, buffer.unique_id, width as i32, height as i32);
    /* Stride: tightly packed. */
    // except: possibly gcd(4,bpp) aligned ?
    let stride = sfdd.view_row_stride.unwrap_or((width * bpp) as u32);
    compute_damaged_segments(&mut rects[..], 6, 128, 0, stride as usize, bpp)
}

fn build_new_format_table(
    vulk: &Arc<Vulkan>,
    sfd: &Rc<RefCell<ShadowFd>>,
    feedback: &mut ObjZwpLinuxDmabufFeedback,
) -> Result<usize, String> {
    /* Identify the remotely supported formats */
    let mut remote_formats = BTreeSet::<u32>::new();
    for t in feedback.tranches.iter() {
        if t.device != feedback.main_device.unwrap() {
            /* Only use main device of compositor; at least one tranche will use it. */
            continue;
        }
        for i in t.entries.iter() {
            let format: (u32, u64) = *feedback
                .format_table
                .get(*i as usize)
                .ok_or("Index error in format list")?;
            remote_formats.insert(format.0);
        }
    }

    /* Identify supported format/modifier pairs */
    let mut modifier_table = BTreeMap::<u32, (Vec<u64>, usize)>::new();
    for f in remote_formats.iter() {
        let mods = vulk.get_supported_modifiers(*f);
        if !mods.is_empty() {
            modifier_table.insert(*f, (mods, 0));
        }
    }

    /* Update format table file */
    let mut format_table_data = Vec::<u8>::new();
    let mut format_table_parsed = Vec::<(u32, u64)>::new();
    let mut ctr = 0;
    for (f, ms) in modifier_table.iter_mut() {
        ms.1 = ctr;
        for m in ms.0.iter() {
            format_table_data.extend_from_slice(&f.to_le_bytes());
            format_table_data.extend_from_slice(&0u32.to_le_bytes());
            format_table_data.extend_from_slice(&m.to_le_bytes());

            format_table_parsed.push((*f, *m));
        }
        ctr += ms.0.len();
    }

    if format_table_parsed.is_empty() {
        return Err(tag!(
            "Failed to build new format table: no formats with common support"
        ));
    }

    let mut b = sfd.borrow_mut();
    let ShadowFdVariant::File(ref mut data) = b.data else {
        panic!("expected file shadowfd");
    };

    let local_fd = memfd::memfd_create(
        c"/waypipe",
        memfd::MemFdCreateFlag::MFD_CLOEXEC | memfd::MemFdCreateFlag::MFD_ALLOW_SEALING,
    )
    .map_err(|x| tag!("Failed to create memfd: {:?}", x))?;
    let sz = format_table_data.len();
    assert!(sz > 0);

    unistd::ftruncate(&local_fd, sz as libc::off_t)
        .map_err(|x| tag!("Failed to resize memfd: {:?}", x))?;
    let mapping: ExternalMapping = ExternalMapping::new(&local_fd, sz, false)
        .map_err(|x| tag!("Failed to mmap fd when building new format table: {}", x))?;
    copy_onto_mapping(&format_table_data[..], &mapping, 0);

    let core = Some(Arc::new(ShadowFdFileCore {
        mem_mirror: Mirror::new(0, false)?,
        mapping,
    }));

    data.buffer_size = sz;
    data.remote_bufsize = sz;
    data.core = core;
    data.fd = local_fd;

    /* Regenerate tranches, roughly matching original _format_ preferences */
    let mut new_tranches = Vec::<DmabufTranche>::new();
    for t in feedback.tranches.iter() {
        if t.device != feedback.main_device.unwrap() {
            continue;
        }

        let mut n = DmabufTranche {
            device: feedback.main_device.unwrap(),
            flags: 0,
            entries: Vec::new(),
        };

        for i in t.entries.iter() {
            let format: (u32, u64) = *feedback
                .format_table
                .get(*i as usize)
                .ok_or("Index error in format list")?;

            /* Record each format in exactly one tranche, preferring earlier tranches;
             * all modifiers for a format are put in the same tranche. */
            if remote_formats.remove(&format.0) {
                if let Some((mods, start)) = modifier_table.get(&format.0) {
                    for i in 0..mods.len() {
                        n.entries.push((start + i).try_into().unwrap());
                    }
                };
            }
        }
        if !n.entries.is_empty() {
            new_tranches.push(n);
        }
    }
    if new_tranches.is_empty() {
        return Err(tag!(
            "Failed to build new format tranches: no formats with common support"
        ));
    }

    feedback.tranches = new_tranches;
    feedback.format_table = format_table_parsed;

    Ok(sz)
}

fn insert_new_object(
    objects: &mut BTreeMap<ObjId, WpObject>,
    id: ObjId,
    obj: WpObject,
) -> Result<(), String> {
    let t = obj.obj_type;
    if let Some(old) = objects.insert(id, obj) {
        return Err(tag!(
            "Creating object of type {:?} with id {}, but object of type {:?} with same id already exists",
            t, id, old.obj_type,
        ));
    }
    Ok(())
}

fn register_generic_new_ids(
    msg: &[u8],
    meth: &WaylandMethod,
    glob: &mut Globals,
) -> Result<(), String> {
    let mut tail = &msg[8..];

    for op in meth.sig {
        match op {
            WaylandArgument::Uint | WaylandArgument::Int | WaylandArgument::Fixed => {
                let _ = parse_u32(&mut tail);
            }
            WaylandArgument::Fd => {
                // do nothing
            }
            WaylandArgument::Object(_) | WaylandArgument::GenericObject => {
                let _ = parse_obj(&mut tail)?;
            }
            WaylandArgument::NewId(new_intf) => {
                let id = parse_obj(&mut tail)?;

                insert_new_object(
                    &mut glob.objects,
                    id,
                    WpObject {
                        obj_type: *new_intf,
                        is_zombie: false,
                        extra: WpExtra::None,
                    },
                )?;
            }
            WaylandArgument::GenericNewId => {
                // order: (string, version, new_id)
                let string = parse_string(&mut tail)?
                    .ok_or_else(|| tag!("New id string should not be null"))?;
                let _version = parse_u32(&mut tail)?;
                let id = parse_obj(&mut tail)?;

                if glob.objects.contains_key(&id) {
                    return Err(tag!("Creating object with id not detected as deleted"));
                }

                if let Some(new_intf) = lookup_intf_by_name(string) {
                    /* Only track recognized object types; messages for
                     * unrecognized types will be ignored. */
                    glob.objects.insert(
                        id,
                        WpObject {
                            obj_type: new_intf,
                            is_zombie: false,
                            extra: WpExtra::None,
                        },
                    );
                }
            }
            WaylandArgument::String => {
                let x = parse_string(&mut tail)?;
                if x.is_none() {
                    return Err(tag!("Received null string where none allowed"));
                }
            }
            WaylandArgument::OptionalString => {
                let _ = parse_string(&mut tail)?;
            }
            WaylandArgument::Array => {
                let _ = parse_array(&mut tail)?;
            }
        }
    }
    Ok(())
}

fn copy_msg(msg: &[u8], dst: &mut &mut [u8]) {
    dst[..msg.len()].copy_from_slice(msg);
    *dst = &mut std::mem::take(dst)[msg.len()..];
}
fn copy_msg_tag_fd(msg: &[u8], dst: &mut &mut [u8], from_channel: bool) -> Result<(), String> {
    dst[..msg.len()].copy_from_slice(msg);
    let mut h2 = u32::from_le_bytes(dst[4..8].try_into().unwrap());

    if from_channel {
        // drop the upper bits of the opcode entirely
        if h2 & (1 << 11) == 0 {
            return Err(tag!("header part {:x} missing fd tag", h2));
        }
        h2 &= !0xff00;
    } else {
        // tag message as using one file descriptor
        h2 = (h2 & !0xff00) | (1 << 11);
    }
    dst[4..8].copy_from_slice(&u32::to_le_bytes(h2));

    *dst = &mut std::mem::take(dst)[msg.len()..];
    Ok(())
}

fn is_server_object(id: ObjId) -> bool {
    id.0 >= 0xff000000
}

fn default_proc_way_msg(
    msg: &[u8],
    dst: &mut &mut [u8],
    meth: &WaylandMethod,
    is_req: bool,
    object_id: ObjId,
    glob: &mut Globals,
) -> Result<ProcMsg, String> {
    if dst.len() < msg.len() {
        // Not enough space to store the message. Then edit the
        // protocol message in place, and stop processing.
        return Ok(ProcMsg::NeedsSpace((msg.len(), 0)));
    }

    register_generic_new_ids(msg, meth, glob)?;

    if meth.destructor {
        if !is_req {
            // Deletion events; process immediately; object is already in map
            glob.objects.remove(&object_id).unwrap();
        } else if is_server_object(object_id) {
            // TODO: is zombie handling necessary, when clients delete server generated objects?
            glob.objects.remove(&object_id).unwrap();
        } else {
            // client object, destructor request: wait until: wl_display.delete_id
        }
    }
    copy_msg(msg, dst);
    Ok(ProcMsg::Done)
}

fn proc_unknown_way_msg(
    msg: &[u8],
    dst: &mut &mut [u8],
    transl: TranslationInfo,
) -> Result<ProcMsg, String> {
    /* Untracked, unidentified object with unknown message */
    if let TranslationInfo::FromChannel((x, y)) = transl {
        let mut header2 = u32::from_le_bytes(msg[4..8].try_into().unwrap());
        let ntagfds = ((header2 & ((1 << 16) - 1)) >> 11) as usize;
        // This is usually safe for simple file transfers (like keymaps),
        // where as long as the sending side recognizes the file it can
        // be replicated OK.
        if ntagfds > 0 {
            error!("Unidentified message has {} fds attached according to other Waypipe instance; blindly transferring them", ntagfds);
        }

        if dst.len() < msg.len() || y.len() + ntagfds > MAX_OUTGOING_FDS {
            // Not enough space to store the message.
            return Ok(ProcMsg::NeedsSpace((msg.len(), ntagfds)));
        }

        if x.len() < ntagfds {
            return Err(tag!("Missing sfd"));
        }
        for sfd in x.iter().take(ntagfds) {
            let b = sfd.borrow();
            if let ShadowFdVariant::File(data) = &b.data {
                if data.pending_apply_tasks > 0 {
                    return Ok(ProcMsg::WaitFor(b.remote_id));
                }
            }
        }

        for _ in 0..ntagfds {
            let sfd = x.pop_front().unwrap();
            y.push_back(sfd);
        }

        /* Copy message to wayland, removing fd tags */
        dst[..msg.len()].copy_from_slice(msg);
        header2 &= !0xf800;
        dst[4..8].copy_from_slice(&u32::to_le_bytes(header2));
    } else {
        if dst.len() < msg.len() {
            // Not enough space to store the message.
            return Ok(ProcMsg::NeedsSpace((msg.len(), 0)));
        }
        /* Copy message to channel, assuming no fds are attached */
        dst[..msg.len()].copy_from_slice(msg);
    }

    *dst = &mut std::mem::take(dst)[msg.len()..];
    Ok(ProcMsg::Done)
}

fn parse_format_table(data: &[u8]) -> Vec<(u32, u64)> {
    let mut t = Vec::new();
    for i in 0..data.len() / 16 {
        let format: u32 = u32::from_le_bytes(data[16 * i..16 * i + 4].try_into().unwrap());
        let modifier: u64 = u64::from_le_bytes(data[16 * i + 8..16 * i + 16].try_into().unwrap());
        t.push((format, modifier));
    }
    t
}

/* Assuming the ShadowFd has file type, does it have pending apply tasks */
fn file_has_pending_apply_tasks(sfd: &RefCell<ShadowFd>) -> Result<bool, String> {
    let b = sfd.borrow();
    let ShadowFdVariant::File(data) = &b.data else {
        // TODO: make this a helper function
        return Err(tag!("ShadowFd is not of file type"));
    };
    Ok(data.pending_apply_tasks > 0)
}

/* Return value is: (sec_diff, nsec_diff) where nsec_diff >= 0 */
fn clock_sub(clock_a: u32, clock_b: u32) -> Result<(i64, u32), String> {
    let mut stamp_1 = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let mut stamp_2 = stamp_1;
    let mut stamp_3 = stamp_1;
    let ca: libc::clockid_t = clock_a.try_into().unwrap();
    let cb: libc::clockid_t = clock_b.try_into().unwrap();
    unsafe {
        // SAFETY: clock_gettime only writes into second argument, which is repr(C)
        // and thus properly aligned
        let ret1 = libc::clock_gettime(ca, &mut stamp_1);
        let ret2 = libc::clock_gettime(cb, &mut stamp_2);
        let ret3 = libc::clock_gettime(ca, &mut stamp_3);
        if ret1 != 0 || ret2 != 0 || ret3 != 0 {
            return Err(tag!(
                "clock_gettime failed for clock {} or {}",
                clock_a,
                clock_b
            ));
        }
    }

    /* Compute midpoint, rounding to -∞ */
    let mid_sec = if stamp_1.tv_sec < stamp_3.tv_sec {
        stamp_1.tv_sec + (stamp_3.tv_sec - stamp_1.tv_sec) / 2
    } else {
        stamp_3.tv_sec + (stamp_1.tv_sec - stamp_3.tv_sec) / 2
    };
    let mid_nsec = if stamp_1.tv_nsec < stamp_3.tv_nsec {
        stamp_1.tv_nsec + (stamp_3.tv_nsec - stamp_1.tv_nsec) / 2
    } else {
        stamp_3.tv_nsec + (stamp_1.tv_nsec - stamp_3.tv_nsec) / 2
    };

    let stamp_avg = libc::timespec {
        tv_sec: mid_sec,
        tv_nsec: mid_nsec,
    };
    let mut tv_sec = stamp_avg
        .tv_sec
        .checked_sub(stamp_2.tv_sec)
        .ok_or_else(|| tag!("overflow"))? as i64;
    let mut tv_nsec = stamp_avg
        .tv_nsec
        .checked_sub(stamp_2.tv_nsec)
        .ok_or_else(|| tag!("overflow"))? as i32;

    if tv_nsec < 0 {
        tv_sec -= 1;
        tv_nsec += 1_000_000_000;
    }
    assert!((0..1_000_000_000).contains(&tv_nsec));

    Ok((tv_sec, tv_nsec as u32))
}
fn time_add(mut a: (u64, u32), b: (i64, u32)) -> Option<(u64, u32)> {
    assert!(a.1 < 1_000_000_000 && b.1 < 1_000_000_000);
    let mut nsec = a.1.checked_add(b.1)?;
    if nsec > 1_000_000_000 {
        nsec -= 1_000_000_000;
        a.0 = a.0.checked_add(1)?;
    }

    // 64-bit time overflow should never happen in practice
    Some((a.0.checked_add_signed(b.0)?, nsec))
}

#[derive(Debug, Eq, PartialEq)]
pub enum ProcMsg {
    Done,
    NeedsSpace((usize, usize)), /* Not enough bytes or fds; value is amount needed */
    /* Wait for all (diff/fill/etc) operations on the given RID to complete. This is only
     * useful for messages coming from the channel whose associated processing might still be
     * in progress. */
    WaitFor(Rid),
}

fn space_le(x: (usize, usize), y: (usize, usize)) -> bool {
    x.0 <= y.0 && x.1 <= y.1
}

macro_rules! check_space {
    ($x:expr, $y:expr, $r:expr) => {
        let space: (usize, usize) = ($x, $y);
        if !space_le(space, $r) {
            return Ok(ProcMsg::NeedsSpace(space));
        }
    };
}

/* Process a Wayland message; typically this just copies the message from the source
 * to the destination buffer. The function returns true if the message was
 * processed, and false if there was not enough space in the destination buffer
 * to do so. */
pub fn process_way_msg(
    msg: &[u8],
    dst: &mut &mut [u8],
    transl: TranslationInfo,
    glob: &mut Globals,
) -> Result<ProcMsg, String> {
    let object_id = ObjId(u32::from_le_bytes(msg[0..4].try_into().unwrap()));
    let header2 = u32::from_le_bytes(msg[4..8].try_into().unwrap());
    let length = (header2 >> 16) as usize;
    assert!(msg.len() == length);
    // drop bits 11-16 from the opcode, as these may encode fds
    let opcode = (header2 & ((1 << 11) - 1)) as usize;

    let (from_channel, outgoing_fds): (bool, usize) = match &transl {
        TranslationInfo::FromChannel((_x, y)) => (true, y.len()),
        TranslationInfo::FromWayland((_x, y)) => (false, y.len()),
    };

    let Some(ref mut obj) = glob.objects.get_mut(&object_id) else {
        return proc_unknown_way_msg(msg, dst, transl);
    };

    let is_req = glob.on_display_side == from_channel;

    let opt_meth: Option<&WaylandMethod> = if is_req {
        INTERFACE_TABLE[obj.obj_type as usize].reqs.get(opcode)
    } else {
        INTERFACE_TABLE[obj.obj_type as usize].evts.get(opcode)
    };
    if opt_meth.is_none() {
        debug!(
            "Method out of range: {}@{}, opcode {}",
            &INTERFACE_TABLE[obj.obj_type as usize].name, object_id, opcode
        );
        return proc_unknown_way_msg(msg, dst, transl);
    }

    let meth = opt_meth.unwrap();
    /* note: this may fail */
    debug!(
        "Processing method: {}@{}.{}",
        &INTERFACE_TABLE[obj.obj_type as usize].name, object_id, meth.name
    );

    assert!(opcode <= u8::MAX as usize);
    let mod_opcode = if is_req {
        MethodId::Request(opcode as u8)
    } else {
        MethodId::Event(opcode as u8)
    };

    let remaining_space = (dst.len(), MAX_OUTGOING_FDS - outgoing_fds);

    match (obj.obj_type, mod_opcode) {
        (WaylandInterface::WlDisplay, OPCODE_WL_DISPLAY_DELETE_ID) => {
            check_space!(msg.len(), 0, remaining_space);

            let object_id = ObjId(parse_evt_wl_display_delete_id(msg)?);
            if object_id == ObjId(1) {
                return Err(tag!("Tried to delete wl_display object"));
            }

            if let Some(removed) = glob.objects.remove(&object_id) {
                /* Cleanup state for some client-created objects. This is done now,
                 * instead of at destroy request time, since the compositor might send
                 * events up until delete_id */

                match removed.extra {
                    WpExtra::WpPresentationFeedback(ref d) => {
                        if let ClockOrObject::Object(pres) = d.clock {
                            /* unregister from wp_presentation's waiting list */
                            let f = glob
                                .objects
                                .get_mut(&pres)
                                .ok_or_else(|| tag!("Failed to lookup presentation object"))?;
                            let WpExtra::WpPresentation(ref mut r) = f.extra else {
                                return Err(tag!("Unexpected object type"));
                            };
                            let ClockOrObjects::Objects(ref mut o) = r.state else {
                                return Err(tag!("Unexpected wp_presentation state"));
                            };
                            o.retain(|x| *x != object_id);
                        }
                    }
                    WpExtra::WpPresentation(ref d) => {
                        if let ClockOrObjects::Objects(ref feedbacks) = d.state {
                            /* unregister all waiting wp_presentation objects */
                            for obj in feedbacks.iter() {
                                let f = glob
                                    .objects
                                    .get_mut(obj)
                                    .ok_or_else(|| tag!("Failed to lookup feedback object"))?;
                                let WpExtra::WpPresentationFeedback(ref mut r) = f.extra else {
                                    return Err(tag!("Unexpected object type"));
                                };
                                r.clock = ClockOrObject::Invalid;
                            }
                        }
                    }
                    _ => (),
                }
            } else {
                debug!("Deleted untracked object");
            }

            copy_msg(msg, dst);

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlCallback, OPCODE_WL_CALLBACK_DONE) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);
            /* This object has a destructor event, and no methods, so
             * it is considered deleted immediately on receipt of the message */
            glob.objects.remove(&object_id);

            // TODO: handle all 'destructor-events' like this, including those for compositor-created objects?
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlShm, OPCODE_WL_SHM_CREATE_POOL) => {
            check_space!(msg.len(), 1, remaining_space);

            let (pool_id, pool_size) = parse_req_wl_shm_create_pool(msg)?;
            let pos_size = pool_size
                .try_into()
                .map_err(|_| tag!("Need nonnegative shm pool size, given {}", pool_size))?;

            let buffer: Rc<RefCell<ShadowFd>> = match transl {
                TranslationInfo::FromChannel((x, y)) => {
                    let sfd = x.pop_front().ok_or_else(|| tag!("Missing fd"))?;
                    y.push_back(sfd.clone());
                    sfd
                }
                TranslationInfo::FromWayland((x, y)) => {
                    // Note: actual file size provided may be larger than pool_size,
                    // since there may be following wl_shm_pool::resize calls

                    let v = translate_shm_fd(
                        x.pop_front().ok_or_else(|| tag!("Missing fd"))?,
                        pos_size,
                        &mut glob.map,
                        &mut glob.max_local_id,
                        false,
                        false,
                    )?;
                    y.push(v.clone());
                    v
                }
            };

            insert_new_object(
                &mut glob.objects,
                pool_id,
                WpObject {
                    obj_type: WaylandInterface::WlShmPool,
                    is_zombie: false,
                    extra: WpExtra::WlShmPool(Box::new(ObjWlShmPool { buffer })),
                },
            )?;

            copy_msg_tag_fd(msg, dst, from_channel)?;

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlShmPool, OPCODE_WL_SHM_POOL_CREATE_BUFFER) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            let (buffer_id, offset, width, height, stride, format) =
                parse_req_wl_shm_pool_create_buffer(msg)?;

            let sfd = if let WpExtra::WlShmPool(ref x) = &obj.extra {
                x.buffer.clone()
            } else {
                return Err(tag!("wl_shm_pool object has invalid extra type"));
            };

            insert_new_object(
                &mut glob.objects,
                buffer_id,
                WpObject {
                    obj_type: WaylandInterface::WlBuffer,
                    is_zombie: false,
                    extra: WpExtra::WlBuffer(Box::new(ObjWlBuffer {
                        sfd,
                        shm_info: Some(ObjWlBufferShm {
                            width,
                            height,
                            format,
                            offset,
                            stride,
                        }),
                        unique_id: glob.max_buffer_uid,
                    })),
                },
            )?;
            glob.max_buffer_uid += 1;

            Ok(ProcMsg::Done)
        }

        (WaylandInterface::WlShmPool, OPCODE_WL_SHM_POOL_RESIZE) => {
            check_space!(msg.len(), 0, remaining_space);

            let WpExtra::WlShmPool(ref x) = &obj.extra else {
                return Err(tag!("wl_shm_pool object has invalid extra type"));
            };

            if file_has_pending_apply_tasks(&x.buffer)? {
                let b = x.buffer.borrow();
                return Ok(ProcMsg::WaitFor(b.remote_id));
            }

            copy_msg(msg, dst);

            if glob.on_display_side {
                /* The application side is responsible for notifying the display
                 * side of the updated buffer size and its contents. */
                return Ok(ProcMsg::Done);
            }

            let size = parse_req_wl_shm_pool_resize(msg)?;
            let new_size: usize = size
                .try_into()
                .map_err(|_| tag!("Invalid buffer size: {}", size))?;

            let x: &mut ShadowFd = &mut x.buffer.borrow_mut();
            if let ShadowFdVariant::File(ref mut y) = x.data {
                y.buffer_size = new_size;

                // extend the mirror and initialize a new mapping
                // requires that no other operations be in progress on mapping
                update_core_for_new_size(&y.fd, new_size, &mut y.core)?;
            }

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlCompositor, OPCODE_WL_COMPOSITOR_CREATE_SURFACE) => {
            check_space!(msg.len(), 0, remaining_space);

            let surf_id = parse_req_wl_compositor_create_surface(msg)?;

            let d = DamageBatch {
                scale: 1,
                transform: WlOutputTransform::Normal,
                damage: Vec::new(),
                damage_buffer: Vec::new(),
                buffer_uid: 0,
            };

            insert_new_object(
                &mut glob.objects,
                surf_id,
                WpObject {
                    obj_type: WaylandInterface::WlSurface,
                    is_zombie: false,
                    extra: WpExtra::WlSurface(Box::new(ObjWlSurface {
                        attached_buffer_id: None,
                        damage_history: [
                            d.clone(),
                            d.clone(),
                            d.clone(),
                            d.clone(),
                            d.clone(),
                            d.clone(),
                            d.clone(),
                        ],
                        acquire_pt: None,
                        release_pt: None,
                    })),
                },
            )?;

            copy_msg(msg, dst);

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlSurface, OPCODE_WL_SURFACE_ATTACH) => {
            check_space!(msg.len(), 0, remaining_space);

            let (buf_id, _x, _y) = parse_req_wl_surface_attach(msg)?;

            if let Some(ref mut object) = glob.objects.get_mut(&object_id) {
                if let WpExtra::WlSurface(ref mut x) = &mut object.extra {
                    x.attached_buffer_id = if buf_id != ObjId(0) {
                        Some(buf_id)
                    } else {
                        None
                    };
                } else {
                    return Err(tag!("Surface object has invalid extra type"));
                }
            } else {
                return Err(tag!("Attaching to nonexistant object"));
            }

            copy_msg(msg, dst);

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlSurface, OPCODE_WL_SURFACE_SET_BUFFER_SCALE) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            let s = parse_req_wl_surface_set_buffer_scale(msg)?;

            if s <= 0 {
                return Err(tag!("wl_surface.set_buffer_scale used nonpositive scale"));
            }

            let WpExtra::WlSurface(ref mut surf) = &mut obj.extra else {
                return Err(tag!("Surface object has invalid extra type"));
            };

            surf.damage_history[0].scale = s as u32;

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlSurface, OPCODE_WL_SURFACE_SET_BUFFER_TRANSFORM) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            let t = parse_req_wl_surface_set_buffer_transform(msg)?;

            let WpExtra::WlSurface(ref mut surf) = &mut obj.extra else {
                return Err(tag!("Surface object has invalid extra type"));
            };
            if t < 0 {
                return Err(tag!("Buffer transform value should be nonnegative"));
            }
            surf.damage_history[0].transform = (t as u32)
                .try_into()
                .map_err(|()| "Not a valid transform type")?;

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlSurface, OPCODE_WL_SURFACE_DAMAGE) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            if glob.on_display_side {
                /* Only the buffers on the application side will be dirty */
                return Ok(ProcMsg::Done);
            }

            let WpExtra::WlSurface(ref mut surf) = &mut obj.extra else {
                return Err(tag!("Surface object has invalid extra type"));
            };

            let (x, y, width, height) = parse_req_wl_surface_damage(msg)?;
            if width <= 0 || height <= 0 {
                /* This doesn't appear to be a protocol error; still, filter out degenerate rectangles */
                error!(
                    "Received degenerate damage rectangle: x={} y={} w={} h={}",
                    x, y, width, height
                );
            } else {
                surf.damage_history[0].damage.push(WlRect {
                    x,
                    y,
                    width,
                    height,
                });
            }

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlSurface, OPCODE_WL_SURFACE_DAMAGE_BUFFER) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            if glob.on_display_side {
                /* Only the buffers on the application side will be dirty */
                return Ok(ProcMsg::Done);
            }

            let WpExtra::WlSurface(ref mut surf) = &mut obj.extra else {
                return Err(tag!("Surface object has invalid extra type"));
            };

            let (x, y, width, height) = parse_req_wl_surface_damage_buffer(msg)?;
            if width <= 0 || height <= 0 {
                /* This doesn't appear to be a protocol error; still, filter out degenerate rectangles */
                error!(
                    "Received degenerate damage rectangle: x={} y={} w={} h={}",
                    x, y, width, height
                );
            } else {
                surf.damage_history[0].damage.push(WlRect {
                    x,
                    y,
                    width,
                    height,
                });
            }

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlSurface, OPCODE_WL_SURFACE_COMMIT) => {
            check_space!(msg.len(), 0, remaining_space);

            let () = parse_req_wl_surface_commit(msg)?;
            let WpExtra::WlSurface(ref x) = obj.extra else {
                return Err(tag!("Surface object has invalid extra type"));
            };
            let opt_buf_id: Option<ObjId> = x.attached_buffer_id;
            if x.acquire_pt.is_some() != x.release_pt.is_some() {
                return Err(tag!("Acquire/release points must both be set"));
            }
            let has_timelines = x.acquire_pt.is_some() || x.release_pt.is_some();

            if from_channel {
                if let Some(buf_id) = opt_buf_id {
                    if let Some(buf) = glob.objects.get(&buf_id) {
                        if let WpExtra::WlBuffer(ref buf_data) = buf.extra {
                            let b = buf_data.sfd.borrow();
                            let apply_count = if let ShadowFdVariant::File(data) = &b.data {
                                data.pending_apply_tasks
                            } else if let ShadowFdVariant::Dmabuf(data) = &b.data {
                                /* Note: there must be a wait _somewhere_ when using timelines, otherwise diffs may be received faster
                                 * than they can be processed. */
                                if has_timelines {
                                    0 /* do not wait for tasks; will signal acquire semaphore on last task completion */
                                } else {
                                    data.pending_apply_tasks
                                }
                            } else {
                                return Err(tag!("Attached buffer is not of file or dmabuf type"));
                            };
                            if apply_count > 0 {
                                return Ok(ProcMsg::WaitFor(b.remote_id));
                            }
                        }
                    }
                }
            }

            copy_msg(msg, dst);

            if glob.on_display_side {
                /* Only the buffers on the application side will be dirty */
                let obj = &mut glob.objects.get_mut(&object_id).unwrap();
                let WpExtra::WlSurface(ref mut x) = &mut obj.extra else {
                    return Err(tag!("Surface object has invalid extra type"));
                };

                if x.acquire_pt.is_some() != x.release_pt.is_some() {
                    return Err(tag!("Acquire/release points must both be set"));
                }
                /* These must be set every swap cycle, so take them */
                let mut acq_pt = None;
                let mut rel_pt = None;
                std::mem::swap(&mut x.acquire_pt, &mut acq_pt);
                std::mem::swap(&mut x.release_pt, &mut rel_pt);

                let opt_buf_id: Option<ObjId> = x.attached_buffer_id;
                if let Some(buf_id) = opt_buf_id {
                    if let Some(buf) = glob.objects.get(&buf_id) {
                        if let WpExtra::WlBuffer(ref buf_data) = buf.extra {
                            let mut sfd = buf_data.sfd.borrow_mut();
                            if let ShadowFdVariant::Dmabuf(ref mut y) = &mut sfd.data {
                                if let Some((pt, timeline)) = acq_pt {
                                    y.acquires.push((pt, timeline));
                                }
                                if let Some((pt, timeline)) = rel_pt {
                                    let mut tsfd = timeline.borrow_mut();
                                    let ShadowFdVariant::Timeline(ref mut timeline_data) =
                                        tsfd.data
                                    else {
                                        panic!("Expected timeline sfd");
                                    };
                                    timeline_data.releases.push((pt, buf_data.sfd.clone()));
                                    let trid = tsfd.remote_id;
                                    drop(tsfd);

                                    y.releases.insert((trid, pt), timeline);
                                }
                                if y.pending_apply_tasks == 0 {
                                    /* All tasks completed before this was processed, so signal immediately */
                                    debug!("Tasks already done, signalling acquires");
                                    signal_timeline_acquires(&mut y.acquires)?;
                                }
                            }
                        }
                    }
                }

                return Ok(ProcMsg::Done);
            }

            let obj = &mut glob.objects.get_mut(&object_id).unwrap();
            let WpExtra::WlSurface(ref mut x) = &mut obj.extra else {
                return Err(tag!("Surface object has invalid extra type"));
            };

            if x.acquire_pt.is_some() != x.release_pt.is_some() {
                return Err(tag!("Acquire/release points must both be set"));
            }
            /* These must be set every swap cycle, so take them */
            let mut acq_pt = None;
            let mut rel_pt = None;
            std::mem::swap(&mut x.acquire_pt, &mut acq_pt);
            std::mem::swap(&mut x.release_pt, &mut rel_pt);

            /* This shifts all entries of x.damage_history right by one */
            // mutable wl_surface reference dropped; now reading from wl_buffer and wl_surface objects

            let buffer_uid = if let Some(buf_id) = opt_buf_id {
                /* Note: this is vulnerable to ABA-type problems when the buffer is
                 * destroyed and a new one with the same id is recreated; however,
                 * this is client misbehavior and Waypipe can silently ignore it,
                 * apply extra damage to the new buffer, and let the compositor
                 * handle it. */
                if let Some(buf) = glob.objects.get(&buf_id) {
                    let obj = &glob.objects.get(&object_id).unwrap();
                    let WpExtra::WlSurface(ref x) = &obj.extra else {
                        unreachable!();
                    };
                    if let WpExtra::WlBuffer(ref buf_data) = buf.extra {
                        let mut sfd = buf_data.sfd.borrow_mut();
                        if let ShadowFdVariant::File(ref mut y) = &mut sfd.data {
                            match &y.damage {
                                Damage::Everything => {}
                                Damage::Nothing => {
                                    y.damage = Damage::Intervals(get_damage_for_shm(buf_data, x));
                                }
                                Damage::Intervals(old) => {
                                    let dmg = get_damage_for_shm(buf_data, x);
                                    y.damage =
                                        Damage::Intervals(union_damage(&old[..], &dmg[..], 128));
                                }
                            }
                        } else if let ShadowFdVariant::Dmabuf(ref mut y) = &mut sfd.data {
                            match &y.damage {
                                Damage::Everything => {}
                                Damage::Nothing => {
                                    y.damage =
                                        Damage::Intervals(get_damage_for_dmabuf(buf_data, y, x));
                                }
                                Damage::Intervals(old) => {
                                    let dmg = get_damage_for_dmabuf(buf_data, y, x);
                                    y.damage =
                                        Damage::Intervals(union_damage(&old[..], &dmg[..], 128));
                                }
                            }

                            /* todo: in theory these should still work with shm buffers */
                            if let Some((pt, timeline)) = acq_pt {
                                y.acquires.push((pt, timeline));
                            }
                            if let Some((pt, timeline)) = rel_pt {
                                let mut tsfd = timeline.borrow_mut();
                                let ShadowFdVariant::Timeline(ref mut timeline_data) = tsfd.data
                                else {
                                    panic!("Expected timeline sfd");
                                };
                                timeline_data.releases.push((pt, buf_data.sfd.clone()));
                                let trid = tsfd.remote_id;
                                drop(tsfd);

                                y.releases.insert((trid, pt), timeline);
                            }
                        } else {
                            return Err(tag!("Expected buffer shadowfd to be of file type"));
                        }

                        Some(buf_data.unique_id)
                    } else {
                        None
                    }
                } else {
                    error!("Attached wl_buffer destroyed before commit: is this a protocol error?. Interpreting as null attachment.");
                    None
                }
            } else {
                /* Attached null, wipe */
                None
            };

            /* acquire mutable reference again */
            let obj = &mut glob.objects.get_mut(&object_id).unwrap();

            let WpExtra::WlSurface(ref mut x) = &mut obj.extra else {
                return Err(tag!("Surface object has invalid extra type"));
            };

            if let Some(b) = buffer_uid {
                /* The damage accumulated for this commit is associated with b, but was not yet updated */
                x.damage_history[0].buffer_uid = b;
                /* Rotate the damage log */
                let mut fresh = DamageBatch {
                    scale: x.damage_history[0].scale,
                    transform: x.damage_history[0].transform,
                    buffer_uid: b,
                    damage: Vec::new(),
                    damage_buffer: Vec::new(),
                };
                std::mem::swap(&mut x.damage_history[6], &mut fresh);
                x.damage_history.swap(5, 6);
                x.damage_history.swap(4, 5);
                x.damage_history.swap(3, 4);
                x.damage_history.swap(2, 3);
                x.damage_history.swap(1, 2);
                x.damage_history.swap(0, 1);
            } else {
                /* Null attachment, wipe history */
                let scale = x.damage_history[0].scale;
                let transform = x.damage_history[0].transform;
                for i in 0..7 {
                    x.damage_history[i] = DamageBatch {
                        scale,
                        transform,
                        buffer_uid: 0,
                        damage: Vec::new(),
                        damage_buffer: Vec::new(),
                    };
                }
            }

            Ok(ProcMsg::Done)
        }
        (
            WaylandInterface::WpLinuxDrmSyncobjManagerV1,
            OPCODE_WP_LINUX_DRM_SYNCOBJ_MANAGER_V1_GET_SURFACE,
        ) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            let (new_id, surf_id) = parse_req_wp_linux_drm_syncobj_manager_v1_get_surface(msg)?;

            // Currently unclear how drm_syncobj interacts with wl_shm-type buffers; in theory,
            // with GPU access to CPU memory, it _should_ be possible.

            insert_new_object(
                &mut glob.objects,
                new_id,
                WpObject {
                    obj_type: WaylandInterface::WpLinuxDrmSyncobjSurfaceV1,
                    is_zombie: false,
                    extra: WpExtra::WpDrmSyncobjSurface(Box::new(ObjWpDrmSyncobjSurface {
                        // TODO: check if ABA problem applies
                        surface: surf_id,
                    })),
                },
            )?;

            Ok(ProcMsg::Done)
        }
        (
            WaylandInterface::WpLinuxDrmSyncobjManagerV1,
            OPCODE_WP_LINUX_DRM_SYNCOBJ_MANAGER_V1_IMPORT_TIMELINE,
        ) => {
            check_space!(msg.len(), 1, remaining_space);

            let new_id = parse_req_wp_linux_drm_syncobj_manager_v1_import_timeline(msg)?;

            let sfd = match transl {
                TranslationInfo::FromChannel((x, y)) => {
                    let sfd = x.pop_front().ok_or_else(|| tag!("Missing sfd"))?;
                    let mut b = sfd.borrow_mut();
                    if let ShadowFdVariant::Timeline(t) = &mut b.data {
                        t.debug_wayland_id = new_id;
                    } else {
                        return Err(tag!("Expected timeline fd"));
                    }
                    drop(b);

                    y.push_back(sfd.clone());
                    sfd
                }
                TranslationInfo::FromWayland((x, y)) => {
                    let v = translate_timeline(
                        x.pop_front().ok_or_else(|| tag!("Missing fd"))?,
                        glob,
                        new_id,
                    )?;
                    y.push(v.clone());
                    v
                }
            };

            insert_new_object(
                &mut glob.objects,
                new_id,
                WpObject {
                    obj_type: WaylandInterface::WpLinuxDrmSyncobjTimelineV1,
                    is_zombie: false,
                    extra: WpExtra::WpDrmSyncobjTimeline(Box::new(ObjWpDrmSyncobjTimeline {
                        // TODO: check if ABA problem applies
                        timeline: sfd,
                    })),
                },
            )?;

            copy_msg(msg, dst);
            Ok(ProcMsg::Done)
        }
        (
            WaylandInterface::WpLinuxDrmSyncobjSurfaceV1,
            OPCODE_WP_LINUX_DRM_SYNCOBJ_SURFACE_V1_SET_ACQUIRE_POINT,
        )
        | (
            WaylandInterface::WpLinuxDrmSyncobjSurfaceV1,
            OPCODE_WP_LINUX_DRM_SYNCOBJ_SURFACE_V1_SET_RELEASE_POINT,
        ) => {
            check_space!(msg.len(), 0, remaining_space);

            let acquire = mod_opcode == OPCODE_WP_LINUX_DRM_SYNCOBJ_SURFACE_V1_SET_ACQUIRE_POINT;

            let (timeline_id, pt_hi, pt_lo) = if acquire {
                parse_req_wp_linux_drm_syncobj_surface_v1_set_acquire_point(msg)?
            } else {
                parse_req_wp_linux_drm_syncobj_surface_v1_set_release_point(msg)?
            };
            let pt = join_u64(pt_hi, pt_lo);

            let WpExtra::WpDrmSyncobjSurface(s) = &obj.extra else {
                panic!("Incorrect extra type");
            };
            let surf_id = s.surface;

            let timeline_obj = glob.objects.get(&timeline_id).ok_or("")?;
            let WpExtra::WpDrmSyncobjTimeline(t) = &timeline_obj.extra else {
                return Err(tag!("Incorrect extra type, expected timeline"));
            };
            let sfd = t.timeline.clone();

            // Currently unclear how drm_syncobj interacts with wl_shm-type buffers; in theory,
            // with GPU access to CPU memory, it _should_ be possible.

            let surface_obj = glob.objects.get_mut(&surf_id).ok_or("")?;
            let WpExtra::WlSurface(ref mut surf) = &mut surface_obj.extra else {
                return Err(tag!("Incorrect extra type, expected surface"));
            };

            if acquire {
                surf.acquire_pt = Some((pt, sfd));
            } else {
                surf.release_pt = Some((pt, sfd));
            }

            copy_msg(msg, dst);
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::ZwpLinuxDmabufV1, OPCODE_ZWP_LINUX_DMABUF_V1_FORMAT) => {
            let format = parse_evt_zwp_linux_dmabuf_v1_format(msg)?;
            let mod_linear = 0;
            if !glob
                .vulkan
                .as_ref()
                .unwrap()
                .supports_format(format, mod_linear)
            {
                /* Drop message, format not supported even for linear modifier */
                return Ok(ProcMsg::Done);
            }
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::ZwpLinuxDmabufV1, OPCODE_ZWP_LINUX_DMABUF_V1_MODIFIER) => {
            let (format, mod_hi, mod_lo) = parse_evt_zwp_linux_dmabuf_v1_modifier(msg)?;
            let modifier = join_u64(mod_hi, mod_lo);
            let vulk = glob.vulkan.as_ref().unwrap();
            if glob.on_display_side {
                /* Restrict the format/modifier pairs to what this instance of Waypipe supports */
                if !vulk.supports_format(format, modifier) {
                    return Ok(ProcMsg::Done);
                }
                check_space!(msg.len(), 0, remaining_space);
                copy_msg(msg, dst);
                Ok(ProcMsg::Done)
            } else {
                /* For each format, replace the modifiers listed with what this instance of Waypipe accepts */
                let WpExtra::ZwpDmabuf(d) = &mut obj.extra else {
                    panic!();
                };
                if d.formats_seen.contains(&format) {
                    return Ok(ProcMsg::Done);
                }
                d.formats_seen.insert(format);

                let mods = vulk.get_supported_modifiers(format);
                check_space!(
                    mods.len() * length_evt_zwp_linux_dmabuf_v1_modifier(),
                    0,
                    remaining_space
                );

                for new_mod in mods {
                    let (nmod_hi, nmod_lo) = split_u64(new_mod);
                    write_evt_zwp_linux_dmabuf_v1_modifier(
                        dst, object_id, format, nmod_hi, nmod_lo,
                    );
                }
                Ok(ProcMsg::Done)
            }
        }

        (WaylandInterface::ZwpLinuxDmabufV1, OPCODE_ZWP_LINUX_DMABUF_V1_CREATE_PARAMS) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            let params_id = parse_req_zwp_linux_dmabuf_v1_create_params(msg)?;

            insert_new_object(
                &mut glob.objects,
                params_id,
                WpObject {
                    obj_type: WaylandInterface::ZwpLinuxBufferParamsV1,
                    is_zombie: false,
                    extra: WpExtra::ZwpDmabufParams(Box::new(ObjZwpLinuxDmabufParams {
                        planes: Vec::new(),
                        dmabuf: None,
                    })),
                },
            )?;

            Ok(ProcMsg::Done)
        }

        (WaylandInterface::ZwpLinuxBufferParamsV1, OPCODE_ZWP_LINUX_BUFFER_PARAMS_V1_ADD) => {
            /* This message will be consumed and recreated later once full information has arrived;
             * it can always be processed */

            let (plane_idx, offset, stride, modifier_hi, modifier_lo) =
                parse_req_zwp_linux_buffer_params_v1_add(msg)?;
            let modifier: u64 = join_u64(modifier_hi, modifier_lo);

            let WpExtra::ZwpDmabufParams(ref mut p) = &mut obj.extra else {
                return Err(tag!("Incorrect extra type")); /* TODO: make a helper method for this, with standardized error ? */
            };

            match transl {
                TranslationInfo::FromChannel((_, _)) => {
                    /* Do nothing: the OpenDMABUF message will be sent at time of ::create or ::create_immed ;
                     * there is no point in dequeing it earlier. Also, there will only be one message, even if
                     * there are multiple planes. */
                }
                TranslationInfo::FromWayland((x, _)) => {
                    let fd = x.pop_front().ok_or_else(|| tag!("Missing fd"))?;

                    p.planes.push(AddDmabufPlane {
                        fd,
                        plane_idx,
                        offset,
                        stride,
                        modifier,
                    });
                }
            };

            Ok(ProcMsg::Done)
        }

        (
            WaylandInterface::ZwpLinuxBufferParamsV1,
            OPCODE_ZWP_LINUX_BUFFER_PARAMS_V1_CREATE_IMMED,
        ) => {
            let WpExtra::ZwpDmabufParams(ref mut params) = &mut obj.extra else {
                return Err(tag!("Incorrect extra type")); /* TODO: make a helper method for this, with standardized error ? */
            };

            let (buffer_id, width, height, drm_format, flags) =
                parse_req_zwp_linux_buffer_params_v1_create_immed(msg)?;
            if width <= 0 || height <= 0 {
                return Err(tag!("DMABUF width or height should be positive"));
            }
            let (width, height) = (width as u32, height as u32);

            if flags != 0 {
                return Err(tag!("DMABUF flags not yet supported"));
            }

            let sfd: Rc<RefCell<ShadowFd>> = match transl {
                TranslationInfo::FromChannel((x, y)) => {
                    let sfd = &x.front().ok_or_else(|| tag!("Missing sfd"))?;
                    let mut b = sfd.borrow_mut();
                    let ShadowFdVariant::Dmabuf(ref mut data) = &mut b.data else {
                        return Err(tag!("Incorrect extra type"));
                    };

                    data.debug_wayland_id = buffer_id;

                    let estimated_msg_len = length_req_zwp_linux_buffer_params_v1_add()
                        * data.export_planes.len()
                        + length_req_zwp_linux_buffer_params_v1_create_immed();
                    check_space!(estimated_msg_len, data.export_planes.len(), remaining_space);

                    for plane in data.export_planes.iter() {
                        let (mod_hi, mod_lo) = split_u64(plane.modifier);
                        write_req_zwp_linux_buffer_params_v1_add(
                            dst,
                            object_id,
                            true,
                            plane.plane_idx,
                            plane.offset,
                            plane.stride,
                            mod_hi,
                            mod_lo,
                        );
                    }
                    copy_msg(msg, dst);

                    drop(b);

                    let sfd = x.pop_front().unwrap();
                    y.push_back(sfd.clone());
                    sfd
                }
                TranslationInfo::FromWayland((_, y)) => {
                    /* Finally process all the SFDs, introduce add messages */
                    let estimated_msg_len = length_req_zwp_linux_buffer_params_v1_add()
                    /* send add messages as-if the buffer were actually linear,
                     * for improved compatibility with older Waypipe */
                    + length_req_zwp_linux_buffer_params_v1_create_immed();
                    check_space!(estimated_msg_len, params.planes.len(), remaining_space);

                    let mut planes = Vec::new();
                    std::mem::swap(&mut params.planes, &mut planes);

                    let sfd = translate_dmabuf_fd(
                        width,
                        height,
                        drm_format,
                        planes,
                        &glob.opts,
                        glob.vulkan.as_ref().unwrap(),
                        &mut glob.max_local_id,
                        &mut glob.map,
                        buffer_id,
                    )?;
                    y.push(sfd.clone());

                    /* Recreate all the 'add' messages at once. While this is technically pointless when Waypipe runs against itself,
                     * since those add messages get ignored and dropped, older versions may care. */
                    let mod_linear: u64 = 0;
                    /* Assuming format is single planar, instruct other end to create a linear buffer;
                     * older Waypipe will do this, and newer will ignore it. */
                    let (mod_hi, mod_lo) = split_u64(mod_linear);

                    let wayl_format = drm_to_wayland(drm_format);
                    let bpp = get_bpp(wayl_format).unwrap();

                    write_req_zwp_linux_buffer_params_v1_add(
                        dst,
                        object_id,
                        false,
                        /* plane idx */ 0,
                        /* offset */ 0,
                        (bpp * width as usize) as u32, // stride as if tightly packed
                        mod_hi,
                        mod_lo,
                    );
                    write_req_zwp_linux_buffer_params_v1_create_immed(
                        dst,
                        object_id,
                        buffer_id,
                        width as i32,
                        height as i32,
                        drm_format,
                        flags,
                    );

                    sfd
                }
            };

            insert_new_object(
                &mut glob.objects,
                buffer_id,
                WpObject {
                    obj_type: WaylandInterface::WlBuffer,
                    is_zombie: false,
                    extra: WpExtra::WlBuffer(Box::new(ObjWlBuffer {
                        sfd,
                        shm_info: None,
                        unique_id: glob.max_buffer_uid,
                    })),
                },
            )?;
            glob.max_buffer_uid += 1;

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::ZwpLinuxBufferParamsV1, OPCODE_ZWP_LINUX_BUFFER_PARAMS_V1_CREATE) => {
            let WpExtra::ZwpDmabufParams(ref mut params) = &mut obj.extra else {
                return Err(tag!("Incorrect extra type")); /* TODO: make a helper method for this, with standardized error ? */
            };
            if params.dmabuf.is_some() {
                return Err(tag!("Can only create dmabuf from params once"));
            }

            let (width, height, drm_format, flags) =
                parse_req_zwp_linux_buffer_params_v1_create(msg)?;
            if width <= 0 || height <= 0 {
                return Err(tag!("DMABUF width or height should be positive"));
            }
            let (width, height) = (width as u32, height as u32);

            let sfd: Rc<RefCell<ShadowFd>> = match transl {
                TranslationInfo::FromChannel((x, y)) => {
                    let sfd = &x.front().ok_or_else(|| tag!("Missing sfd"))?;
                    let mut b = sfd.borrow_mut();
                    let ShadowFdVariant::Dmabuf(ref mut data) = &mut b.data else {
                        return Err(tag!("Incorrect extra type"));
                    };
                    let estimated_msg_len = length_req_zwp_linux_buffer_params_v1_add()
                        * data.export_planes.len()
                        + length_req_zwp_linux_buffer_params_v1_create_immed();
                    check_space!(estimated_msg_len, data.export_planes.len(), remaining_space);

                    for plane in data.export_planes.iter() {
                        let (mod_hi, mod_lo) = split_u64(plane.modifier);
                        write_req_zwp_linux_buffer_params_v1_add(
                            dst,
                            object_id,
                            true,
                            plane.plane_idx,
                            plane.offset,
                            plane.stride,
                            mod_hi,
                            mod_lo,
                        );
                    }
                    copy_msg(msg, dst);

                    drop(b);

                    let sfd = x.pop_front().unwrap();
                    y.push_back(sfd.clone());
                    sfd
                }
                TranslationInfo::FromWayland((_, y)) => {
                    /* Finally process all the SFDs, introduce add messages */
                    let estimated_msg_len = length_req_zwp_linux_buffer_params_v1_add()
                    /* send add messages as-if the buffer were actually linear,
                     * for improved compatibility with older Waypipe */
                    + length_req_zwp_linux_buffer_params_v1_create_immed();
                    check_space!(estimated_msg_len, params.planes.len(), remaining_space);

                    let mut planes = Vec::new();
                    std::mem::swap(&mut params.planes, &mut planes);

                    let sfd = translate_dmabuf_fd(
                        width,
                        height,
                        drm_format,
                        planes,
                        &glob.opts,
                        glob.vulkan.as_ref().unwrap(),
                        &mut glob.max_local_id,
                        &mut glob.map,
                        ObjId(0),
                    )?;
                    y.push(sfd.clone());

                    /* Recreate all the 'add' messages at once. While this is technically pointless when Waypipe runs against itself,
                     * since those add messages get ignored and dropped, older versions may care. */
                    let mod_linear: u64 = 0;
                    /* Assuming format is single planar, instruct other end to create a linear buffer;
                     * older Waypipe will do this, and newer will ignore it. */
                    let (mod_hi, mod_lo) = split_u64(mod_linear);

                    let wayl_format = drm_to_wayland(drm_format);
                    let bpp = get_bpp(wayl_format).unwrap();

                    write_req_zwp_linux_buffer_params_v1_add(
                        dst,
                        object_id,
                        false,
                        /* plane idx */ 0,
                        /* offset */ 0,
                        (bpp * width as usize) as u32, // stride as if tightly packed
                        mod_hi,
                        mod_lo,
                    );
                    write_req_zwp_linux_buffer_params_v1_create(
                        dst,
                        object_id,
                        width as i32,
                        height as i32,
                        drm_format,
                        flags,
                    );

                    sfd
                }
            };
            params.dmabuf = Some(sfd);
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::ZwpLinuxBufferParamsV1, OPCODE_ZWP_LINUX_BUFFER_PARAMS_V1_FAILED) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            /* Discard reference to dmabuf now; compositor rejected it */
            let WpExtra::ZwpDmabufParams(ref mut params) = &mut obj.extra else {
                return Err(tag!("Incorrect extra type"));
            };
            params.dmabuf = None;
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::ZwpLinuxBufferParamsV1, OPCODE_ZWP_LINUX_BUFFER_PARAMS_V1_CREATED) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            let obj_id = parse_evt_zwp_linux_buffer_params_v1_created(msg)?;
            let WpExtra::ZwpDmabufParams(ref mut params) = &mut obj.extra else {
                return Err(tag!("Incorrect extra type"));
            };

            let mut osfd = None;
            std::mem::swap(&mut params.dmabuf, &mut osfd);
            let Some(sfd) = osfd else {
                return Err(tag!(
                    "zwp_linux_buffer_params_v1::created event must follow ::create request and must occur at most once"
                ));
            };

            let mut b = sfd.borrow_mut();
            let ShadowFdVariant::Dmabuf(ref mut d) = b.data else {
                return Err(tag!("Incorrect shadowfd type"));
            };
            d.debug_wayland_id = obj_id;
            drop(b);

            insert_new_object(
                &mut glob.objects,
                obj_id,
                WpObject {
                    obj_type: WaylandInterface::WlBuffer,
                    is_zombie: false,
                    extra: WpExtra::WlBuffer(Box::new(ObjWlBuffer {
                        sfd,
                        shm_info: None,
                        unique_id: glob.max_buffer_uid,
                    })),
                },
            )?;
            glob.max_buffer_uid += 1;

            Ok(ProcMsg::Done)
        }

        (WaylandInterface::ZwpLinuxDmabufV1, OPCODE_ZWP_LINUX_DMABUF_V1_GET_DEFAULT_FEEDBACK)
        | (WaylandInterface::ZwpLinuxDmabufV1, OPCODE_ZWP_LINUX_DMABUF_V1_GET_SURFACE_FEEDBACK) => {
            check_space!(msg.len(), 0, remaining_space);
            copy_msg(msg, dst);

            let feedback_id = if mod_opcode == OPCODE_ZWP_LINUX_DMABUF_V1_GET_DEFAULT_FEEDBACK {
                parse_req_zwp_linux_dmabuf_v1_get_default_feedback(msg)?
            } else {
                let (f, _surface) = parse_req_zwp_linux_dmabuf_v1_get_surface_feedback(msg)?;
                f
            };

            insert_new_object(
                &mut glob.objects,
                feedback_id,
                WpObject {
                    obj_type: WaylandInterface::ZwpLinuxDmabufFeedbackV1,
                    is_zombie: false,
                    extra: WpExtra::ZwpDmabufFeedback(Box::new(ObjZwpLinuxDmabufFeedback {
                        table_fd: None,
                        table_sfd: None,
                        main_device: None,
                        format_table: Vec::new(),
                        tranches: Vec::new(),
                        current: DmabufTranche {
                            flags: 0,
                            entries: Vec::new(),
                            device: 0,
                        },
                    })),
                },
            )?;

            Ok(ProcMsg::Done)
        }

        (
            WaylandInterface::ZwpLinuxDmabufFeedbackV1,
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_FORMAT_TABLE,
        ) => {
            let table_size = parse_evt_zwp_linux_dmabuf_feedback_v1_format_table(msg)?;

            let WpExtra::ZwpDmabufFeedback(ref mut f) = &mut obj.extra else {
                return Err(tag!("Expected object to have dmabuf_feedback data"));
            };

            /* Store data from message, and drop it; will replay later */
            match transl {
                TranslationInfo::FromChannel((x, _)) => {
                    let sfd = x.pop_front().ok_or_else(|| tag!("Missing sfd"))?;
                    f.table_sfd = Some(sfd);
                }
                TranslationInfo::FromWayland((x, _)) => {
                    let fd = x.pop_front().unwrap();
                    f.table_fd = Some((fd, table_size));
                }
            };

            Ok(ProcMsg::Done)
        }

        (
            WaylandInterface::ZwpLinuxDmabufFeedbackV1,
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_MAIN_DEVICE,
        ) => {
            let WpExtra::ZwpDmabufFeedback(ref mut feedback) = &mut obj.extra else {
                return Err(tag!("Unexpected object extra type"));
            };

            let dev = parse_evt_zwp_linux_dmabuf_feedback_v1_main_device(msg)?;
            assert!(dev.len() == 8);

            feedback.main_device = Some(u64::from_le_bytes(dev.try_into().unwrap()));

            Ok(ProcMsg::Done)
        }
        (
            WaylandInterface::ZwpLinuxDmabufFeedbackV1,
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_FLAGS,
        ) => {
            let WpExtra::ZwpDmabufFeedback(ref mut feedback) = &mut obj.extra else {
                return Err(tag!("Unexpected object extra type"));
            };

            let flags = parse_evt_zwp_linux_dmabuf_feedback_v1_tranche_flags(msg)?;
            feedback.current.flags = flags;

            Ok(ProcMsg::Done)
        }
        (
            WaylandInterface::ZwpLinuxDmabufFeedbackV1,
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_TARGET_DEVICE,
        ) => {
            let WpExtra::ZwpDmabufFeedback(ref mut feedback) = &mut obj.extra else {
                return Err(tag!("Unexpected object extra type"));
            };

            let dev = parse_evt_zwp_linux_dmabuf_feedback_v1_tranche_target_device(msg)?;
            assert!(dev.len() == 8);
            feedback.current.device = u64::from_le_bytes(dev.try_into().unwrap());

            Ok(ProcMsg::Done)
        }
        (
            WaylandInterface::ZwpLinuxDmabufFeedbackV1,
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_FORMATS,
        ) => {
            let WpExtra::ZwpDmabufFeedback(ref mut feedback) = &mut obj.extra else {
                return Err(tag!("Unexpected object extra type"));
            };

            let fmts = parse_evt_zwp_linux_dmabuf_feedback_v1_tranche_formats(msg)?;
            if fmts.len() % 2 != 0 {
                return Err(tag!("Format array not of even length"));
            }
            feedback.current.entries = fmts
                .chunks(2)
                .map(|x| u16::from_le_bytes(x.try_into().unwrap()))
                .collect();

            Ok(ProcMsg::Done)
        }

        (
            WaylandInterface::ZwpLinuxDmabufFeedbackV1,
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_DONE,
        ) => {
            let WpExtra::ZwpDmabufFeedback(ref mut feedback) = &mut obj.extra else {
                return Err(tag!("Unexpected object extra type"));
            };
            feedback.tranches.push(DmabufTranche {
                flags: 0,
                entries: Vec::new(),
                device: 0,
            });
            std::mem::swap(feedback.tranches.last_mut().unwrap(), &mut feedback.current);
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::ZwpLinuxDmabufFeedbackV1, OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_DONE) => {
            let WpExtra::ZwpDmabufFeedback(ref mut feedback) = &mut obj.extra else {
                return Err(tag!("Unexpected object extra type"));
            };

            if glob.on_display_side && glob.vulkan.is_none() {
                /* Try to initialize Vulkan, now that we know _a_ device to use */
                // TODO: handle cases where surface_feedback disagrees with default_feedback, or
                // where only surface_feedback is asked for and never default_feedback
                let Some(dev) = feedback.main_device else {
                    return Err(tag!(
                        "zwp_linux_dmabuf_feedback_v1 did not provide a device"
                    ));
                };

                glob.vulkan = Some(setup_vulkan(
                    Some(dev),
                    glob.opts.video.format.is_some(),
                    glob.opts.debug,
                    !glob.opts.force_sw_decoding,
                    !glob.opts.force_sw_encoding,
                )?);
            }

            let dev_len = std::mem::size_of::<u64>();

            /* This can be an overestimate */
            let mut space_est = 0;
            if feedback.table_sfd.is_some() || feedback.table_fd.is_some() {
                space_est += length_evt_zwp_linux_dmabuf_feedback_v1_format_table();
            }
            space_est += length_evt_zwp_linux_dmabuf_feedback_v1_main_device(dev_len);
            space_est += length_evt_zwp_linux_dmabuf_feedback_v1_done();

            for t in feedback.tranches.iter() {
                space_est += length_evt_zwp_linux_dmabuf_feedback_v1_tranche_done()
                    + length_evt_zwp_linux_dmabuf_feedback_v1_tranche_flags()
                    + length_evt_zwp_linux_dmabuf_feedback_v1_tranche_target_device(dev_len)
                    + length_evt_zwp_linux_dmabuf_feedback_v1_tranche_formats(
                        t.entries.len() * std::mem::size_of::<u16>(),
                    );
            }

            check_space!(
                space_est,
                feedback.table_sfd.is_some() as usize,
                remaining_space
            );

            match transl {
                TranslationInfo::FromChannel((_, y)) => {
                    if let Some(sfd) = &feedback.table_sfd {
                        if file_has_pending_apply_tasks(sfd)? {
                            let b = sfd.borrow();
                            return Ok(ProcMsg::WaitFor(b.remote_id));
                        }
                    }

                    /* Read table into format_table, if one was sent. */
                    let mut osfd = None;
                    std::mem::swap(&mut feedback.table_sfd, &mut osfd);
                    if let Some(sfd) = osfd {
                        let b = sfd.borrow();
                        let ShadowFdVariant::File(ref x) = &b.data else {
                            panic!("Expected buffer shadowfd to be of file type");
                        };

                        let table_size = x.buffer_size;
                        let mut data = vec![0; table_size];
                        copy_from_mapping(&mut data, &x.core.as_ref().unwrap().mapping, 0);
                        drop(b);
                        //
                        feedback.format_table = parse_format_table(&data[..]);

                        let new_size =
                            build_new_format_table(glob.vulkan.as_ref().unwrap(), &sfd, feedback)?;

                        y.push_back(sfd);

                        write_evt_zwp_linux_dmabuf_feedback_v1_format_table(
                            dst,
                            object_id,
                            true,
                            new_size.try_into().unwrap(),
                        )
                    }
                }
                TranslationInfo::FromWayland((_, y)) => {
                    /* Translate and read table into format_table, if one was sent. */
                    let mut ofd = None;
                    std::mem::swap(&mut feedback.table_fd, &mut ofd);
                    if let Some((fd, table_size)) = ofd {
                        let sfd = translate_shm_fd(
                            fd,
                            table_size as usize,
                            &mut glob.map,
                            &mut glob.max_local_id,
                            true,
                            true,
                        )?;

                        let b = sfd.borrow();
                        let ShadowFdVariant::File(ref x) = &b.data else {
                            panic!("Expected buffer shadowfd to be of file type");
                        };
                        let mut data = vec![0; table_size as usize];
                        copy_from_mapping(&mut data, &x.core.as_ref().unwrap().mapping, 0);
                        drop(b);

                        y.push(sfd);

                        write_evt_zwp_linux_dmabuf_feedback_v1_format_table(
                            dst, object_id, false, table_size,
                        );

                        feedback.format_table = parse_format_table(&data[..]);
                    }
                }
            }

            /* Write messages, filtering as necessary. */
            let vulk: &Vulkan = glob.vulkan.as_ref().unwrap();
            let dev_id: u64 = vulk.get_device();
            write_evt_zwp_linux_dmabuf_feedback_v1_main_device(
                dst,
                object_id,
                dev_id.to_le_bytes().as_slice(),
            );
            for t in feedback.tranches.iter() {
                let mut evec = Vec::<u8>::new();
                for i in t.entries.iter() {
                    let format: (u32, u64) = *feedback
                        .format_table
                        .get(*i as usize)
                        .ok_or("Index error in format list")?;
                    /* This check is _technically_ redundant on application side, where
                     * the format table is remade via build_new_format_table */
                    if vulk.supports_format(format.0, format.1) {
                        let a = i.to_le_bytes();
                        evec.push(a[0]);
                        evec.push(a[1]);
                    }
                }
                if !evec.is_empty() {
                    write_evt_zwp_linux_dmabuf_feedback_v1_tranche_target_device(
                        dst,
                        object_id,
                        dev_id.to_le_bytes().as_slice(),
                    );
                    write_evt_zwp_linux_dmabuf_feedback_v1_tranche_flags(dst, object_id, t.flags);
                    write_evt_zwp_linux_dmabuf_feedback_v1_tranche_formats(
                        dst,
                        object_id,
                        &evec[..],
                    );
                    write_evt_zwp_linux_dmabuf_feedback_v1_tranche_done(dst, object_id);
                }
            }
            write_evt_zwp_linux_dmabuf_feedback_v1_done(dst, object_id);

            /* Cleanup */
            feedback.tranches = Vec::new();
            feedback.current.entries = Vec::new();

            Ok(ProcMsg::Done)
        }

        (WaylandInterface::WlKeyboard, OPCODE_WL_KEYBOARD_KEYMAP) => {
            check_space!(msg.len(), 1, remaining_space);

            let (_, keymap_size) = parse_evt_wl_keyboard_keymap(msg)?;
            let pos_size = keymap_size
                .try_into()
                .map_err(|_| tag!("Need nonnegative key map size, not {:?}", keymap_size))?;

            match transl {
                TranslationInfo::FromChannel((x, y)) => {
                    let sfd = &x.front().ok_or_else(|| tag!("Missing fd"))?;
                    let rid = sfd.borrow().remote_id;
                    if file_has_pending_apply_tasks(&sfd)? {
                        return Ok(ProcMsg::WaitFor(rid));
                    }
                    y.push_back(x.pop_front().unwrap());
                }
                TranslationInfo::FromWayland((x, y)) => {
                    let v = translate_shm_fd(
                        x.pop_front().ok_or_else(|| tag!("Missing fd"))?,
                        pos_size,
                        &mut glob.map,
                        &mut glob.max_local_id,
                        true,
                        true,
                    )?;
                    y.push(v);
                }
            };

            copy_msg_tag_fd(msg, dst, from_channel)?;

            Ok(ProcMsg::Done)
        }
        (
            WaylandInterface::ZwpPrimarySelectionSourceV1,
            OPCODE_ZWP_PRIMARY_SELECTION_SOURCE_V1_SEND,
        )
        | (
            WaylandInterface::ZwpPrimarySelectionOfferV1,
            OPCODE_ZWP_PRIMARY_SELECTION_OFFER_V1_RECEIVE,
        )
        | (WaylandInterface::GtkPrimarySelectionSource, OPCODE_GTK_PRIMARY_SELECTION_SOURCE_SEND)
        | (
            WaylandInterface::GtkPrimarySelectionOffer,
            OPCODE_GTK_PRIMARY_SELECTION_OFFER_RECEIVE,
        )
        | (WaylandInterface::ExtDataControlSourceV1, OPCODE_EXT_DATA_CONTROL_SOURCE_V1_SEND)
        | (WaylandInterface::ExtDataControlOfferV1, OPCODE_EXT_DATA_CONTROL_OFFER_V1_RECEIVE)
        | (WaylandInterface::ZwlrDataControlSourceV1, OPCODE_ZWLR_DATA_CONTROL_SOURCE_V1_SEND)
        | (WaylandInterface::ZwlrDataControlOfferV1, OPCODE_ZWLR_DATA_CONTROL_OFFER_V1_RECEIVE)
        | (WaylandInterface::WlDataSource, OPCODE_WL_DATA_SOURCE_SEND)
        | (WaylandInterface::WlDataOffer, OPCODE_WL_DATA_OFFER_RECEIVE) => {
            check_space!(msg.len(), 1, remaining_space);

            /* Message format is mimetype + fd, and the mimetype doesn't matter
             * for Waypipe */

            match transl {
                TranslationInfo::FromChannel((x, y)) => {
                    let sfd = x.pop_front().ok_or_else(|| tag!("Missing sfd"))?;
                    y.push_back(sfd);
                }
                TranslationInfo::FromWayland((x, y)) => {
                    let v = translate_pipe_fd(
                        x.pop_front().ok_or_else(|| tag!("Missing fd"))?,
                        glob,
                        true, // reading from channel
                    )?;
                    y.push(v);
                }
            };

            copy_msg_tag_fd(msg, dst, from_channel)?;

            Ok(ProcMsg::Done)
        }
        (WaylandInterface::XdgToplevel, OPCODE_XDG_TOPLEVEL_SET_TITLE) => {
            let title = parse_req_xdg_toplevel_set_title(msg)?;
            let prefix = glob.opts.title_prefix.as_bytes();

            let space_needed = length_req_xdg_toplevel_set_title(title.len() + prefix.len());
            check_space!(space_needed, 0, remaining_space);

            // TODO: direct manipulation is appropriate here, because the output
            // already provides the necessary space
            let mut concat: Vec<u8> = Vec::new();
            concat.extend_from_slice(prefix);
            concat.extend_from_slice(title);
            write_req_xdg_toplevel_set_title(dst, object_id, &concat);
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WpPresentationFeedback, OPCODE_WP_PRESENTATION_FEEDBACK_PRESENTED) => {
            check_space!(msg.len(), 0, remaining_space);
            let (tv_sec_hi, tv_sec_lo, tv_nsec, refresh, seq_hi, seq_lo, flags) =
                parse_evt_wp_presentation_feedback_presented(msg)?;
            let WpExtra::WpPresentationFeedback(ref x) = obj.extra else {
                unreachable!();
            };
            let ClockOrObject::Clock(clock_id) = x.clock else {
                return Err(tag!(
                    "Feedback presented for {} before clock id available; state {:?}",
                    object_id,
                    x.clock,
                ));
            };

            /* Translate timestamp to/from a shared clock (CLOCK_REALTIME) */
            let tv_sec = join_u64(tv_sec_hi, tv_sec_lo);
            let realtime = libc::CLOCK_REALTIME as u32;
            let (new_sec, new_nsec) = if glob.on_display_side {
                /* Convert from clock_id to CLOCK_REALTIME */
                let (diff_sec, diff_nsec) = clock_sub(realtime, clock_id)?;
                time_add((tv_sec, tv_nsec), (diff_sec, diff_nsec))
                    .ok_or_else(|| tag!("overflow"))?
            } else {
                /* Convert from CLOCK_REALTIME to clock_id */
                let (diff_sec, diff_nsec) = clock_sub(clock_id, realtime)?;
                time_add((tv_sec, tv_nsec), (diff_sec, diff_nsec))
                    .ok_or_else(|| tag!("overflow"))?
            };
            let (new_sec_hi, new_sec_lo) = split_u64(new_sec);

            write_evt_wp_presentation_feedback_presented(
                dst, object_id, new_sec_hi, new_sec_lo, new_nsec, refresh, seq_hi, seq_lo, flags,
            );
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WpPresentation, OPCODE_WP_PRESENTATION_FEEDBACK) => {
            check_space!(msg.len(), 0, remaining_space);
            let (_surf_id, callback_id) = parse_req_wp_presentation_feedback(msg)?;
            let WpExtra::WpPresentation(ref mut x) = obj.extra else {
                unreachable!();
            };
            let clock = match x.state {
                ClockOrObjects::Clock(x) => ClockOrObject::Clock(x),
                ClockOrObjects::Objects(ref mut v) => {
                    v.push(callback_id);
                    ClockOrObject::Object(object_id)
                }
            };

            insert_new_object(
                &mut glob.objects,
                callback_id,
                WpObject {
                    obj_type: WaylandInterface::WpPresentationFeedback,
                    is_zombie: false,
                    extra: WpExtra::WpPresentationFeedback(Box::new(ObjWpPresentationFeedback {
                        clock,
                    })),
                },
            )?;

            copy_msg(msg, dst);
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WpPresentation, OPCODE_WP_PRESENTATION_CLOCK_ID) => {
            check_space!(msg.len(), 0, remaining_space);
            let clock_id = parse_evt_wp_presentation_clock_id(msg)?;
            let WpExtra::WpPresentation(ref mut x) = obj.extra else {
                unreachable!();
            };
            let mut state = ClockOrObjects::Clock(clock_id);
            std::mem::swap(&mut x.state, &mut state);
            match state {
                ClockOrObjects::Clock(x) => {
                    // The protocol text requires that clock_id must be sent on bind, and may not change,
                    // but does not forbid the same clock_id message to be sent multiple times.
                    if x != clock_id {
                        return Err(tag!(
                            "wp_presentation clock already set to {}, but receiving invalid change to {}",
                            x,
                            clock_id
                        ));
                    }
                }
                ClockOrObjects::Objects(v) => {
                    for id in &v {
                        let obj = glob.objects.get_mut(id).ok_or_else(|| {
                            tag!("Failed to lookup presentation feedback object {}", id)
                        })?;
                        let WpExtra::WpPresentationFeedback(ref mut d) = obj.extra else {
                            return Err(tag!("Expected wp_presentation_feedback object"));
                        };
                        let ClockOrObject::Object(o) = d.clock else {
                            return Err(tag!(
                                "wp_presentation_feedback object in state: {:?}",
                                d.clock
                            ));
                        };
                        if o != object_id {
                            return Err(tag!("Object id mismatch"));
                        }
                        d.clock = ClockOrObject::Clock(clock_id);
                    }
                }
            }

            copy_msg(msg, dst);
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlRegistry, OPCODE_WL_REGISTRY_GLOBAL) => {
            // filter out events
            let (name, intf, mut version) = parse_evt_wl_registry_global(msg)?;

            /* Note: nothing that gets filtered out should ever be removed,
             * so it is not necessary to track interface name codes for global_remove */
            let blacklist: &'static [&'static [u8]] = &[
                b"wl_drm", /* very old/deprecated API: linux-dmabuf-v4 replaces */
                b"wp_drm_lease_device_v1",
                b"zwlr_export_dmabuf_manager_v1",
                b"zwp_linux_explicit_synchronization_v1", // outdated, uses fences instead of timelines
                b"wp_security_context_manager_v1",        // sends socket listen fd over network
            ];

            /* Version downgrading */
            if intf == b"wl_shm" {
                let max_v = INTERFACE_TABLE[WaylandInterface::WlShm as usize].version;
                if version > max_v {
                    version = max_v;
                    debug!("Downgrading wl_shm version from {} to {}", version, max_v);
                }
            }
            if blacklist.contains(&intf) {
                /* Drop interface entirely */
                debug!("Dropping interface: {}", escape_wl_name(intf));
                return Ok(ProcMsg::Done);
            }

            if intf == b"zwp_linux_dmabuf_v1" {
                if glob.opts.no_gpu {
                    debug!(
                        "no-gpu option: Dropping interface: {}",
                        escape_wl_name(intf)
                    );
                    return Ok(ProcMsg::Done);
                }

                let max_v = INTERFACE_TABLE[WaylandInterface::ZwpLinuxDmabufV1 as usize].version;
                /* only support versions >= 4, which provide the local drm node to use */
                if version < 4 {
                    debug!("Dropping interface: {}", escape_wl_name(intf));
                    return Ok(ProcMsg::Done);
                }
                if version > max_v {
                    version = max_v;
                    debug!(
                        "Downgrading zwp_linux_dmabuf_v1 version from {} to {}",
                        version, max_v
                    );
                }
            }
            check_space!(msg.len(), 0, remaining_space);
            write_evt_wl_registry_global(dst, object_id, name, intf, version);
            Ok(ProcMsg::Done)
        }
        (WaylandInterface::WlRegistry, OPCODE_WL_REGISTRY_BIND) => {
            // filter out events
            let (_id, name, version, oid) = parse_req_wl_registry_bind(msg)?;
            if name == b"zwp_linux_dmabuf_v1" {
                if glob.vulkan.is_none() && glob.on_display_side && version < 4 {
                    debug!(
                        "Client bound zwp_linux_dmabuf_v1 at version {} older than 4,
                           using best-or-specified drm node",
                        version
                    );
                }

                /* Client side (or server if version < 4): initialize Vulkan,
                 * choosing best device or specified device. */
                if glob.vulkan.is_none() && (!glob.on_display_side || version < 4) {
                    let video = glob.opts.video.format.is_some();
                    let dev = if let Some(node) = &glob.opts.drm_node {
                        /* Pick specified device */
                        Some(get_dev_for_drm_node_path(node)?)
                    } else {
                        /* Pick best device */
                        None
                    };
                    glob.vulkan = Some(setup_vulkan(
                        dev,
                        video,
                        glob.opts.debug,
                        !glob.opts.force_sw_decoding,
                        !glob.opts.force_sw_encoding,
                    )?);
                }

                check_space!(msg.len(), 0, remaining_space);
                copy_msg(msg, dst);
                insert_new_object(
                    &mut glob.objects,
                    oid,
                    WpObject {
                        obj_type: WaylandInterface::ZwpLinuxDmabufV1,
                        is_zombie: false,
                        extra: WpExtra::ZwpDmabuf(Box::new(ObjZwpLinuxDmabuf {
                            formats_seen: BTreeSet::new(),
                        })),
                    },
                )?;
                return Ok(ProcMsg::Done);
            }
            if name == b"wp_presentation" {
                check_space!(msg.len(), 0, remaining_space);
                copy_msg(msg, dst);
                insert_new_object(
                    &mut glob.objects,
                    oid,
                    WpObject {
                        obj_type: WaylandInterface::WpPresentation,
                        is_zombie: false,
                        extra: WpExtra::WpPresentation(Box::new(ObjWpPresentation {
                            state: ClockOrObjects::Objects(Vec::new()),
                        })),
                    },
                )?;
                return Ok(ProcMsg::Done);
            }

            /* create new global objects */
            default_proc_way_msg(msg, dst, meth, is_req, object_id, glob)
        }

        _ => {
            // Default handling: copy message, and create IDs
            default_proc_way_msg(msg, dst, meth, is_req, object_id, glob)
        }
    }
}

pub fn setup_object_map() -> BTreeMap<ObjId, WpObject> {
    let mut map = BTreeMap::new();
    map.insert(
        ObjId(1),
        WpObject {
            obj_type: WaylandInterface::WlDisplay,
            is_zombie: false,
            extra: WpExtra::None,
        },
    );
    map
}
