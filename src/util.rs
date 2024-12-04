/* SPDX-License-Identifier: GPL-3.0-or-later */

use std::fmt;
use std::fmt::Write;
use std::str::FromStr;

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum WmsgType {
    /** Send over a set of Wayland protocol messages. Preceding messages
     * must create or update file descriptors and inject file descriptors
     * to the queue. */
    Protocol = 0, // header uint32_t, then protocol messages
    /** Inject file descriptors into the receiver's buffer, for use by the
     * protocol parser. */
    InjectRIDs = 1, // header uint32_t, then fds
    /** Create a new shared memory file of the given size.
     * Format: \ref wmsg_open_file */
    OpenFile = 2,
    /** Provide a new (larger) size for the file buffer.
     * Format: \ref wmsg_open_file */
    ExtendFile = 3,
    /** Create a new DMABUF with the given size and \ref dmabuf_slice_data.
     * Format: \ref wmsg_open_dmabuf */
    OpenDMABUF = 4,
    /** Fill the region of the file with the folllowing data. The data
     * should be compressed according to the global compression option.
     * Format: \ref wmsg_buffer_fill */
    BufferFill = 5,
    /** Apply a diff to the file. The diff contents may be compressed.
     * Format: \ref wmsg_buffer_diff */
    BufferDiff = 6,
    /** Create a new pipe, with the given remote R/W status */
    OpenIRPipe = 7, // wmsg_basic
    OpenIWPipe = 8, // wmsg_basic
    OpenRWPipe = 9, // wmsg_basic
    /** Transfer data to the pipe */
    PipeTransfer = 10, // wmsg_basic
    /** Shutdown the read end of the pipe that waypipe uses. */
    PipeShutdownR = 11, // wmsg_basic
    /** Shutdown the write end of the pipe that waypipe uses. */
    PipeShutdownW = 12, // wmsg_basic
    /** Create a DMABUF (with following data parameters) that will be used
     * to produce/consume video frames. Format: \ref wmsg_open_dmabuf.
     * Deprecated and may be disabled/removed in the future. */
    OpenDMAVidSrc = 13,
    OpenDMAVidDst = 14,
    /** Send a packet of video data to the destination */
    SendDMAVidPacket = 15, // wmsg_basic
    /** Acknowledge that a given number of messages has been received, so
     * that the sender of those messages no longer needs to store them
     * for replaying in case of reconnection. Format: \ref wmsg_ack */
    AckNblocks = 16,
    /** When restarting a connection, indicate the number of the message
     * which will be sent next. Format: \ref wmsg_restart */
    Restart = 17, // wmsg_restart
    /** When the remote program is closing. Format: only the header */
    Close = 18,
    /** Create a DMABUF (with following data parameters) that will be used
     * to produce/consume video frames. Format: \ref wmsg_open_dmavid */
    OpenDMAVidSrcV2 = 19,
    OpenDMAVidDstV2 = 20,
    /* Create a DRM syncobj timeline semaphore. Format: header, u64-le initial point */
    OpenTimeline = 21,
    /* Signal the indicated DRM syncobj timeline semaphore.  Format: header, u64-le initial point. */
    SignalTimeline = 22,
    /* Sent as the first message from the client to reveal the negotiated wire protocol
     * version. Format: header, u32 version field */
    Version = 23,
}

pub fn align(x: usize, y: usize) -> usize {
    y * ((x.checked_add(y - 1).unwrap()) / y)
}
pub fn align4(x: usize) -> usize {
    align(x, 4)
}
pub fn cat2x4(x: [u8; 4], y: [u8; 4]) -> [u8; 8] {
    [x[0], x[1], x[2], x[3], y[0], y[1], y[2], y[3]]
}
pub fn cat3x4(x: [u8; 4], y: [u8; 4], z: [u8; 4]) -> [u8; 12] {
    [
        x[0], x[1], x[2], x[3], y[0], y[1], y[2], y[3], z[0], z[1], z[2], z[3],
    ]
}
pub fn cat4x4(x: [u8; 4], y: [u8; 4], z: [u8; 4], a: [u8; 4]) -> [u8; 16] {
    [
        x[0], x[1], x[2], x[3], y[0], y[1], y[2], y[3], z[0], z[1], z[2], z[3], a[0], a[1], a[2],
        a[3],
    ]
}
pub fn split_interval(lo: u32, hi: u32, nparts: u32, index: u32) -> u32 {
    assert!(nparts < 1 << 15 && hi - lo < 1 << 31);
    lo + index * ((hi - lo) / nparts) + (index * ((hi - lo) % nparts)) / nparts
}
pub fn ceildiv(v: u32, u: u32) -> u32 {
    // todo: bounds checking
    (v + u - 1) / u
}
/* Split u64 into high (32:63) and low (0:31) parts */
pub fn split_u64(x: u64) -> (u32, u32) {
    ((x >> 32) as u32, x as u32)
}
pub fn join_u64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

pub fn build_wmsg_header(typ: WmsgType, len: usize) -> u32 {
    u32::try_from(len).unwrap().checked_mul(1 << 5).unwrap() | (typ as u32)
}

/** The size excludes trailing padding (to multiple of 4). */
pub fn parse_wmsg_header(header: u32) -> Option<(usize, WmsgType)> {
    let code = header & ((1 << 5) - 1);
    let len = (header >> 5) as usize;
    let t = match code {
        0 => WmsgType::Protocol,
        1 => WmsgType::InjectRIDs,
        2 => WmsgType::OpenFile,
        3 => WmsgType::ExtendFile,
        4 => WmsgType::OpenDMABUF,
        5 => WmsgType::BufferFill,
        6 => WmsgType::BufferDiff,
        7 => WmsgType::OpenIRPipe,
        8 => WmsgType::OpenIWPipe,
        9 => WmsgType::OpenRWPipe,
        10 => WmsgType::PipeTransfer,
        11 => WmsgType::PipeShutdownR,
        12 => WmsgType::PipeShutdownW,
        13 => WmsgType::OpenDMAVidSrc,
        14 => WmsgType::OpenDMAVidDst,
        15 => WmsgType::SendDMAVidPacket,
        16 => WmsgType::AckNblocks,
        17 => WmsgType::Restart,
        18 => WmsgType::Close,
        19 => WmsgType::OpenDMAVidSrcV2,
        20 => WmsgType::OpenDMAVidDstV2,
        21 => WmsgType::OpenTimeline,
        22 => WmsgType::SignalTimeline,
        23 => WmsgType::Version,
        _ => {
            return None;
        }
    };
    Some((len, t))
}

pub fn retain_err<T, F, E>(x: &mut Vec<T>, mut f: F) -> Result<(), E>
where
    F: FnMut(&mut T) -> Result<bool, E>,
{
    let mut e: Result<(), E> = Ok(());
    x.retain_mut(|y| match f(y) {
        Ok(b) => b,
        Err(x) => {
            e = Err(x);
            /* It doesn't matter whether we keep or exit in this case */
            true
        }
    });
    e
}

/* Wayland interface names should consist of [a-zA-Z0-9_]. Escape all unexpected characters. */
pub fn escape_wl_name(name: &[u8]) -> String {
    let mut s = String::new();
    for c in name {
        match *c {
            b'_' | b'a'..=b'z' | b'0'..=b'9' | b'A'..=b'Z' => {
                s.push(char::from_u32(*c as u32).unwrap())
            }
            _ => {
                write!(s, "\\x{:02x}", *c).unwrap();
            }
        }
    }
    s
}

pub fn escape_non_ascii_printable(name: &[u8]) -> String {
    let mut s = String::new();
    for c in name {
        match *c {
            b' '..=b'~' => s.push(char::from_u32(*c as u32).unwrap()),
            _ => {
                write!(s, "\\x{:02x}", *c).unwrap();
            }
        }
    }
    s
}

/* A heap-allocated 64-aligned array */
pub struct AlignedArray {
    data: *mut u8,
    size: usize,
}
unsafe impl Send for AlignedArray {}
unsafe impl Sync for AlignedArray {}

impl AlignedArray {
    pub fn new(size: usize) -> AlignedArray {
        if size == 0 {
            AlignedArray {
                data: std::ptr::null_mut(),
                size: 0,
            }
        } else {
            let layout = std::alloc::Layout::from_size_align(size, 64).unwrap();

            unsafe {
                // SAFETY: layout size was checked to be > 0
                let mem = std::alloc::alloc_zeroed(layout).cast::<u8>();
                assert!(!mem.is_null());
                AlignedArray { data: mem, size }
            }
        }
    }
    /* Returns (ptr, len); ptr is promised to be 64 aligned */
    pub fn get_parts(&self) -> (*mut u8, usize) {
        (self.data, self.size)
    }
    pub fn get_mut(&mut self) -> &mut [u8] {
        if self.size == 0 {
            return &mut [];
        }
        unsafe {
            // SAFETY: self.data is not null since size > 0 was checked
            // data is 64-aligned, and only 1-alignment needed for u8
            // size matches allocated amount
            // &mut self argument ensures no other calls to get_mut() can
            // overlap in lifespan, so slice is not otherwise accessed; other unsafe
            // users of AlignedArray should enforce similar behavior
            &mut *std::ptr::slice_from_raw_parts_mut(self.data, self.size)
        }
    }
    pub fn get(&self) -> &[u8] {
        if self.size == 0 {
            return &[];
        }
        unsafe {
            // SAFETY: bounds OK else allocation would fail, todo
            &*std::ptr::slice_from_raw_parts(self.data, self.size)
        }
    }
}
impl Drop for AlignedArray {
    fn drop(&mut self) {
        if self.size > 0 {
            let layout = std::alloc::Layout::from_size_align(self.size, 64).unwrap();
            unsafe {
                // SAFETY: self.data is not null and was allocated with the same layout
                std::alloc::dealloc(self.data, layout);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Lz4(i8),
    Zstd(i8),
}
impl FromStr for Compression {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const FAILURE: &str = "Compression should have format: 'none', 'lz4[=#]', or 'zstd[=#]'";
        if s == "none" {
            Ok(Compression::None)
        } else if s.starts_with("lz4") {
            let lvl: i8;
            if s == "lz4" {
                lvl = 0;
            } else if let Some(suffix) = s.strip_prefix("lz4=") {
                lvl = suffix.parse::<i8>().map_err(|_| FAILURE)?;
            } else {
                return Err(FAILURE);
            }

            Ok(Compression::Lz4(lvl))
        } else if s.starts_with("zstd") {
            let lvl: i8;
            if s == "zstd" {
                lvl = 0;
            } else if let Some(suffix) = s.strip_prefix("zstd=") {
                lvl = suffix.parse::<i8>().map_err(|_| FAILURE)?;
            } else {
                return Err(FAILURE);
            }

            Ok(Compression::Zstd(lvl))
        } else {
            Err(FAILURE)
        }
    }
}
impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Compression::None => write!(f, "none"),
            Compression::Lz4(i) => {
                if *i == 0 {
                    write!(f, "lz4")
                } else {
                    write!(f, "lz4={}", i)
                }
            }
            Compression::Zstd(i) => {
                if *i == 0 {
                    write!(f, "zstd")
                } else {
                    write!(f, "zstd={}", i)
                }
            }
        }
    }
}
#[test]
fn compression_enum_roundtrip() {
    assert_eq!(
        Compression::from_str(&Compression::None.to_string()),
        Ok(Compression::None)
    );
    for i in i8::MIN..=i8::MAX {
        assert_eq!(
            Compression::from_str(&Compression::Lz4(i).to_string()),
            Ok(Compression::Lz4(i))
        );
        assert_eq!(
            Compression::from_str(&Compression::Zstd(i).to_string()),
            Ok(Compression::Zstd(i))
        );
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum VideoFormat {
    /* Values are used in wire protocol */
    H264 = 0,
    VP9 = 1,
    AV1 = 2,
}
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct VideoSetting {
    /* If not set, no video encoding done */
    pub format: Option<VideoFormat>,
    /* If not set, default */
    pub bits_per_frame: Option<f32>,
}
impl FromStr for VideoSetting {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const FAILURE: &str =
            "Video spec should be comma-separated with strings: 'none', 'h264', 'bpf=<real>'";
        let mut f = VideoSetting {
            format: None,
            bits_per_frame: None,
        };

        for chunk in s.split_terminator(',') {
            if chunk == "none" {
                f.format = None;
            } else if chunk == "hw" || chunk == "sw" {
                /* ignore */
            } else if chunk == "h264" {
                f.format = Some(VideoFormat::H264);
            } else if chunk == "vp9" {
                f.format = Some(VideoFormat::VP9);
            } else if chunk == "av1" {
                f.format = Some(VideoFormat::AV1);
            } else if let Some(suffix) = chunk.strip_prefix("bpf=") {
                f.bits_per_frame = Some(suffix.parse::<f32>().map_err(|_| FAILURE)?);
            } else {
                return Err(FAILURE);
            }
        }
        Ok(f)
    }
}
impl fmt::Display for VideoSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(fmt) = &self.format {
            match fmt {
                VideoFormat::H264 => write!(f, "h264")?,
                VideoFormat::AV1 => write!(f, "av1")?,
                VideoFormat::VP9 => write!(f, "vp9")?,
            };
        } else {
            write!(f, "none")?;
        }

        if let Some(bpf) = self.bits_per_frame {
            write!(f, ",bpf={}", bpf)
        } else {
            write!(f, "")
        }
    }
}
#[test]
fn video_setting_roundtrip() {
    let examples = [
        VideoSetting {
            format: None,
            bits_per_frame: None,
        },
        VideoSetting {
            format: None,
            bits_per_frame: Some(1e9),
        },
        VideoSetting {
            format: Some(VideoFormat::H264),
            bits_per_frame: Some(100.0),
        },
        VideoSetting {
            format: Some(VideoFormat::VP9),
            bits_per_frame: Some(4321.0),
        },
        VideoSetting {
            format: Some(VideoFormat::H264),
            bits_per_frame: None,
        },
        VideoSetting {
            format: Some(VideoFormat::AV1),
            bits_per_frame: None,
        },
    ];
    for v in examples {
        assert_eq!(VideoSetting::from_str(&VideoSetting::to_string(&v)), Ok(v));
    }
}
