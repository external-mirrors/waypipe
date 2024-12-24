/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! Support for DMABUFs and timeline semaphores (using Vulkan) */
#![cfg(feature = "dmabuf")]
use crate::tag;
use crate::util::*;
#[cfg(feature = "video")]
pub use crate::video::*;
use crate::wayland_gen::*;
use ash::*;
use log::debug;
use nix::{errno, libc};
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString, OsString};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::ptr::{slice_from_raw_parts, slice_from_raw_parts_mut};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug)]
pub struct AddDmabufPlane {
    pub fd: OwnedFd,
    pub plane_idx: u32,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

#[derive(Debug)]
pub struct ModifierData {
    pub modifier: u64,
    pub plane_count: u32,
    pub max_size_transfer: (usize, usize),
    pub max_size_store_and_sample: Option<(usize, usize)>,
}

#[derive(Debug)]
pub struct FormatData {
    pub modifiers: Vec<ModifierData>,
}

pub struct VulkanQueue {
    /* mutable globals for which access must be externally synchronized */
    pub queue: vk::Queue,
    /* The last semaphore value planned to be signalled by a submission to the queue */
    pub last_semaphore_value: u64,
}

pub struct VulkanQueueGuard<'a> {
    pub inner: MutexGuard<'a, VulkanQueue>,
    vulk: &'a Vulkan,
}

pub struct Vulkan {
    _entry: Entry,
    instance: Instance,
    _physdev: vk::PhysicalDevice,
    /** Timeline semaphore; when it reaches 'queue.last_semaphore_value', all preceding work using
     * the semaphore is done */
    pub semaphore: vk::Semaphore,
    _semaphore_fd: OwnedFd,
    semaphore_drm_handle: u32,
    drm_fd: OwnedFd,
    event_fd: OwnedFd,

    /** The compute+transfer queue to use. Do NOT access via queue.lock() -- use
     * vulkan_lock_queue() instead, to also ensure ffmpeg is locked out of using
     * the queue. */
    queue: Mutex<VulkanQueue>,

    #[cfg(feature = "video")]
    pub video: Option<VulkanVideo>,

    pub dev: Device,
    get_modifier: ext::image_drm_format_modifier::Device,
    get_mem_reqs2: khr::get_memory_requirements2::Device,
    bind_mem2: khr::bind_memory2::Device,
    ext_mem_fd: khr::external_memory_fd::Device,
    pub timeline_semaphore: khr::timeline_semaphore::Device,
    ext_semaphore_fd: khr::external_semaphore_fd::Device,

    pub formats: BTreeMap<vk::Format, FormatData>,
    device_id: u64,
    pub queue_family: u32,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
}

pub struct VulkanTimelineSemaphore {
    pub vulk: Arc<Vulkan>,
    pub semaphore: vk::Semaphore,
    semaphore_drm_handle: u32,
    event_fd: OwnedFd,
}

pub struct VulkanCommandPool {
    pub vulk: Arc<Vulkan>,
    pub pool: Mutex<vk::CommandPool>,
}

pub struct VulkanDmabufInner {
    // TODO: need to store metadata about all pending operations to the dmabuf
    pub image_layout: vk::ImageLayout,
}

pub struct VulkanDmabuf {
    // No RefCell -- unsafe is used throughout anyway, exclusivity is not needed, and no recursion should be done
    pub vulk: Arc<Vulkan>,

    pub image: vk::Image,
    // todo: store memory, to be able to free it properly when the VulkanDmabuf is dropped?
    // also: the file descriptors probably have a separate lifespan; check this
    pub width: usize, // TODO: make u32, for better source and Vulkan compatiblity?
    pub height: usize,
    // note: the drm_format should never be needed after casting to nearest Vulkan approximation
    pub vk_format: vk::Format,

    /* True iff vkImage was created with storage and sample usage */
    pub can_store_and_sample: bool,

    // todo: use a <=4 vector size optimization without any heap allocation
    memory_planes: Vec<(vk::DeviceMemory, u32, u32)>, /* mem / offset / stride */

    pub inner: Mutex<VulkanDmabufInner>,
}

struct VulkanBufferInner {
    data: *mut c_void,
    reader_count: usize,
    has_writer: bool,
}

/* A mapped staging buffer, either for use when reading or writing data */
pub struct VulkanBuffer {
    pub vulk: Arc<Vulkan>,

    // todo: central handling of vk::Buffer objects, to allow bump-allocating
    // segments from a larger central buffer.
    pub buffer: vk::Buffer,
    mem: vk::DeviceMemory,

    pub memory_len: u64,
    pub buffer_len: u64,
    /* Mutex-wrapped to ensure only one referent can read/write from data at a time */
    inner: Mutex<VulkanBufferInner>,
}
unsafe impl Send for VulkanBuffer {}
unsafe impl Sync for VulkanBuffer {}

// TODO: VulkanCopyHandle must live at least as long as the originating VulkanCopy and VulkanDmabuf
pub struct VulkanCopyHandle {
    vulk: Arc<Vulkan>,
    /* Copy operation is between these two objects */
    _image: Arc<VulkanDmabuf>,
    _buffer: Arc<VulkanBuffer>,
    pool: Arc<VulkanCommandPool>,

    // note: not safe to reuse unless image is still alive

    // TODO: not safe to free a 'pending' command buffer; give Vulkan itself a list of copy-handles?
    cb: vk::CommandBuffer,
    // on the queue's timeline semaphore
    completion_time_point: u64,
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        unsafe {
            #[cfg(feature = "video")]
            {
                if let Some(ref v) = self.video {
                    destroy_video(&self.dev, v);
                }
                /* Drop video, if present */
                self.video = None;
            }

            // These are probably only safe if operations using them have completed; need
            // a centralized registry
            self.dev.destroy_semaphore(self.semaphore, None);
            self.dev.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
impl Drop for VulkanQueue {
    fn drop(&mut self) {}
}

impl Drop for VulkanCommandPool {
    fn drop(&mut self) {
        unsafe {
            let p = self.pool.lock().unwrap();
            self.vulk.dev.destroy_command_pool(*p, None);
        }
    }
}
impl Drop for VulkanDmabuf {
    fn drop(&mut self) {
        unsafe {
            self.vulk.dev.destroy_image(self.image, None);
            for (mem, _offset, _stride) in &self.memory_planes {
                self.vulk.dev.free_memory(*mem, None);
            }
        }
        // The Arc<Vulkan>> should keep Vulkan alive until after VulkanDmabuf is dropped
    }
}

impl Drop for VulkanTimelineSemaphore {
    fn drop(&mut self) {
        unsafe {
            drm_syncobj_destroy(&self.vulk.drm_fd, self.semaphore_drm_handle).unwrap();
            // SAFETY: only if semaphore is not being used
            self.vulk.dev.destroy_semaphore(self.semaphore, None);

            // event_fd cleanup is automatic
        }
    }
}

impl Drop for VulkanBuffer {
    fn drop(&mut self) {
        unsafe {
            assert!(self.inner.lock().unwrap().reader_count == 0);
            assert!(!self.inner.lock().unwrap().has_writer);
            self.vulk.dev.destroy_buffer(self.buffer, None);
            self.vulk.dev.unmap_memory(self.mem);
            self.vulk.dev.free_memory(self.mem, None);
        }
    }
}
impl Drop for VulkanCopyHandle {
    fn drop(&mut self) {
        let cmd_pool = self.pool.pool.lock().unwrap();
        unsafe {
            /* Verify that the command buffer execution has completed; if not, panic, as it's a program error */
            if let Ok(counter) = self
                .vulk
                .timeline_semaphore
                .get_semaphore_counter_value(self.vulk.semaphore)
            {
                assert!(
                    counter >= self.completion_time_point,
                    "copy handle deleted at {} >!= {}; dropped too early?",
                    counter,
                    self.completion_time_point
                );
            }
            self.vulk.dev.free_command_buffers(*cmd_pool, &[self.cb]);
        }
    }
}

pub fn vulkan_lock_queue(vulk: &Vulkan) -> VulkanQueueGuard<'_> {
    /* Lock order: vulk.queue before video lock */
    let inner = vulk.queue.lock().unwrap();
    #[cfg(feature = "video")]
    if let Some(ref v) = vulk.video {
        unsafe {
            video_lock_queue(v, vulk.queue_family);
        }
    }
    VulkanQueueGuard { inner, vulk }
}
impl Drop for VulkanQueueGuard<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "video")]
        if let Some(ref v) = self.vulk.video {
            unsafe {
                video_unlock_queue(v, self.vulk.queue_family);
            }
        }
    }
}

/** Check whether a given extension is available in the list with the given version. */
fn exts_has_prop(exts: &[vk::ExtensionProperties], name: &CStr, version: u32) -> bool {
    exts.iter()
        .any(|x| x.extension_name_as_c_str().unwrap() == name && x.spec_version >= version)
}

// Additional information for vulkan formats
pub struct FormatLayoutInfo {
    pub bpp: usize,
    pub planes: usize,
    // TODO: while the number of _memory_ planes can be looked up by modifier
    // (e.g., to account for CCS planes), need to track number of logical planes
    // (e.g.: vk::ImageAspectFlags::PLANE_0 vs vk::ImageAspectFlags::MEMORY_PLANE_0_EXT)

    // todo: subsampling in vulkan requires width/height to be multiples of 2 sometimes;
    // would need to bump up canonical size to match
}

const fn fourcc(a: char, b: char, c: char, d: char) -> u32 {
    u32::from_le_bytes([(a as u8), (b as u8), (c as u8), (d as u8)])
}

// TODO: determine if it is worth it to deduplicate shm and dmabuf format information.
// (the code pathways will probably become very different.)
const SUPPORTED_FORMAT_LIST: &[vk::Format] = &[
    vk::Format::R4G4B4A4_UNORM_PACK16,
    vk::Format::R5G6B5_UNORM_PACK16,
    vk::Format::R8_UNORM,
    vk::Format::R8G8_UNORM,
    vk::Format::R8G8B8_UNORM,
    vk::Format::R8G8B8A8_UNORM,
    vk::Format::A2R10G10B10_UNORM_PACK32,
    vk::Format::R16_UNORM,
    vk::Format::R16G16_UNORM,
    vk::Format::R16G16B16A16_UNORM,
    vk::Format::R16G16B16A16_SFLOAT,
    vk::Format::G8_B8_R8_3PLANE_444_UNORM,
];

pub fn get_vulkan_info(f: vk::Format) -> FormatLayoutInfo {
    match f {
        vk::Format::R4G4B4A4_UNORM_PACK16 => FormatLayoutInfo { bpp: 2, planes: 1 },
        vk::Format::R5G6B5_UNORM_PACK16 => FormatLayoutInfo { bpp: 2, planes: 1 },
        vk::Format::R8_UNORM => FormatLayoutInfo { bpp: 1, planes: 1 },
        vk::Format::R8G8_UNORM => FormatLayoutInfo { bpp: 2, planes: 1 },
        vk::Format::R8G8B8_UNORM => FormatLayoutInfo { bpp: 3, planes: 1 },
        vk::Format::R8G8B8A8_UNORM => FormatLayoutInfo { bpp: 4, planes: 1 },
        vk::Format::A2R10G10B10_UNORM_PACK32 => FormatLayoutInfo { bpp: 4, planes: 1 },
        vk::Format::R16_UNORM => FormatLayoutInfo { bpp: 2, planes: 1 },
        vk::Format::R16G16_UNORM => FormatLayoutInfo { bpp: 4, planes: 1 },
        vk::Format::R16G16B16A16_UNORM => FormatLayoutInfo { bpp: 8, planes: 1 },
        vk::Format::R16G16B16A16_SFLOAT => FormatLayoutInfo { bpp: 8, planes: 1 },

        vk::Format::G8B8G8R8_422_UNORM => FormatLayoutInfo { bpp: 2, planes: 1 },
        vk::Format::G8_B8_R8_3PLANE_420_UNORM => FormatLayoutInfo { bpp: 2, planes: 3 },
        vk::Format::G8_B8_R8_3PLANE_422_UNORM => FormatLayoutInfo { bpp: 2, planes: 3 },
        vk::Format::G8_B8_R8_3PLANE_444_UNORM => FormatLayoutInfo { bpp: 2, planes: 3 },
        vk::Format::G8_B8R8_2PLANE_420_UNORM => FormatLayoutInfo { bpp: 2, planes: 2 },
        vk::Format::G8_B8R8_2PLANE_422_UNORM => FormatLayoutInfo { bpp: 2, planes: 2 },
        vk::Format::G16_B16R16_2PLANE_420_UNORM => FormatLayoutInfo { bpp: 2, planes: 2 },
        vk::Format::G16_B16_R16_3PLANE_444_UNORM => FormatLayoutInfo { bpp: 2, planes: 3 },
        _ => unreachable!("Format {:?} should have been implemented", f),
    }
}

/* Wayland and DRM differ in encodings for Argb8888 and Xrgb8888 only */
pub fn drm_to_wayland(drm_format: u32) -> u32 {
    if drm_format == fourcc('A', 'R', '2', '4') {
        WlShmFormat::Argb8888 as u32
    } else if drm_format == fourcc('X', 'R', '2', '4') {
        WlShmFormat::Xrgb8888 as u32
    } else {
        drm_format
    }
}

#[allow(dead_code)]
#[cfg(any(test, feature = "test_proto"))]
pub const fn wayland_to_drm(wl_format: WlShmFormat) -> u32 {
    match wl_format {
        WlShmFormat::Argb8888 => fourcc('A', 'R', '2', '4'),
        WlShmFormat::Xrgb8888 => fourcc('X', 'R', '2', '4'),
        _ => wl_format as u32,
    }
}

/* Convert a DRM fourcc format to a canonical Vulkan format with an equivalent layout
 * (but possibly different channel names -- those can be either ignored or fixed by swizzling.) The other direction should _never_ be necessary. */
pub fn drm_to_vulkan(drm_format: u32) -> Option<vk::Format> {
    use WlShmFormat::*;
    if drm_format == 0 || drm_format == 1 {
        /* not a DRM format, and clients should not send this for DMABUFs */
        return None;
    }

    /* First, convert to Wayland format */
    let shm_format = if let Ok(shm_format) = drm_format.try_into() {
        shm_format
    } else {
        /* Unfortunately wayland uses different values for Argb8888, Xrgb8888 */
        if drm_format == fourcc('A', 'R', '2', '4') {
            Argb8888
        } else if drm_format == fourcc('X', 'R', '2', '4') {
            Xrgb8888
        } else {
            return None;
        }
    };
    // TODO: is it safe to bundle multiple original channels into one? e.g. RGB233 => R8_UNORM
    // TODO: UNORM vs interpreting everything as UINT
    Some(match shm_format {
        Xrgb4444 | Xbgr4444 | Rgbx4444 | Bgrx4444 | Argb4444 | Abgr4444 | Rgba4444 | Bgra4444 => {
            vk::Format::R4G4B4A4_UNORM_PACK16
        }

        Rgb565 | Bgr565 => vk::Format::R5G6B5_UNORM_PACK16,

        /* 8-bit RGB */
        R8 | C8 | D8 => vk::Format::R8_UNORM,
        Rg88 | Gr88 => vk::Format::R8G8_UNORM,
        Rgb888 | Bgr888 => vk::Format::R8G8B8_UNORM,
        Argb8888 | Abgr8888 | Xrgb8888 | Xbgr8888 | Rgba8888 | Bgra8888 | Rgbx8888 | Bgrx8888
        | Ayuv | Avuy8888 | Xyuv8888 => vk::Format::R8G8B8A8_UNORM,
        /* 10-bit RGB */
        Argb2101010 | Xrgb2101010 | Abgr2101010 | Xbgr2101010 => {
            vk::Format::A2R10G10B10_UNORM_PACK32
        }
        /* 16-bit RGB */
        R16 => vk::Format::R16_UNORM,
        Rg1616 | Gr1616 => vk::Format::R16G16_UNORM,
        Xrgb16161616 | Xbgr16161616 | Argb16161616 | Abgr16161616 => vk::Format::R16G16B16A16_UNORM,
        Xrgb16161616f | Xbgr16161616f | Argb16161616f | Abgr16161616f => {
            vk::Format::R16G16B16A16_SFLOAT
        }

        Yuyv | Yvyu => vk::Format::G8B8G8R8_422_UNORM,
        Yuv420 => vk::Format::G8_B8_R8_3PLANE_420_UNORM,
        Yuv422 => vk::Format::G8_B8_R8_3PLANE_422_UNORM,
        Yuv444 => vk::Format::G8_B8_R8_3PLANE_444_UNORM,
        Nv12 => vk::Format::G8_B8R8_2PLANE_420_UNORM,
        Nv16 => vk::Format::G8_B8R8_2PLANE_422_UNORM,
        P016 => vk::Format::G16_B16R16_2PLANE_420_UNORM,
        Q401 => vk::Format::G16_B16_R16_3PLANE_444_UNORM,

        _ => return None,
    })
}

/* Definitions from drm.h */
const fn drm_iowr<T>(code: u8) -> u32 {
    /* linux/ioctl.h */
    let typ = 'd' as u32;
    let size = std::mem::size_of::<T>() as u32;
    let dir = 0x1 | 0x2;
    (code as u32) | (typ << 8) | (size << 16) | (dir << 30)
}
const DRM_IOCTL_SYNCOBJ_DESTROY: u32 = drm_iowr::<DrmSyncobjDestroy>(0xC0);
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: u32 = drm_iowr::<DrmSyncobjHandle>(0xC2);
const DRM_IOCTL_SYNCOBJ_EVENTFD: u32 = drm_iowr::<DrmSyncobjEventFd>(0xCF);

#[repr(C)]
struct DrmSyncobjDestroy {
    handle: u32,
    pad: u32,
}
#[repr(C)]
struct DrmSyncobjHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}
#[repr(C)]
struct DrmSyncobjEventFd {
    handle: u32,
    flags: u32,
    point: u64,
    fd: i32,
    pad: u32,
}

fn drm_open_render(minor: u32) -> Result<OwnedFd, String> {
    let mut path = OsString::new();
    path.push("/dev/dri/renderD");
    path.push(OsString::from(minor.to_string()));
    let p = PathBuf::from(path);
    let raw_fd = nix::fcntl::open(
        &p,
        nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|x| tag!("Failed to open drm node fd at '{:?}': {}", p, x))?;
    Ok(unsafe {
        // SAFETY: fd was just created, was checked valid, and is recorded nowhere else
        OwnedFd::from_raw_fd(raw_fd)
    })
}

/* Requirements: for the specific ioctl used, arg must be properly
 * aligned, have the right type, and have the correct lifespan */
unsafe fn ioctl_loop(
    drm_fd: &OwnedFd,
    code: u32,
    arg: *mut c_void,
    about: &str,
) -> Result<(), String> {
    let req = code as libc::c_ulong;
    loop {
        let ret = libc::ioctl(drm_fd.as_raw_fd(), req, arg);
        let errno = errno::Errno::last_raw();
        if ret == 0 {
            return Ok(());
        } else if (errno == errno::Errno::EINTR as i32) || (errno == errno::Errno::EAGAIN as i32) {
            continue;
        } else {
            return Err(tag!("ioctl {:x} ({}) failed: {}", code, about, errno));
        }
    }
}

fn drm_syncobj_eventfd(
    drm_fd: &OwnedFd,
    event_fd: &OwnedFd,
    handle: u32,
    point: u64,
) -> Result<(), String> {
    let mut x = DrmSyncobjEventFd {
        handle,
        flags: 0, // or: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE
        point,
        fd: event_fd.as_raw_fd(),
        pad: 0,
    };
    unsafe {
        // SAFETY: x is repr(C), x has proper type for the ioctl,
        // and the ioctl does not use the pointer after the call
        ioctl_loop(
            drm_fd,
            DRM_IOCTL_SYNCOBJ_EVENTFD,
            &mut x as *mut _ as *mut c_void,
            "eventfd",
        )
    }
}
fn drm_syncobj_fd_to_handle(drm_fd: &OwnedFd, syncobj_fd: &OwnedFd) -> Result<u32, String> {
    let mut x = DrmSyncobjHandle {
        handle: 0,
        flags: 0,
        fd: syncobj_fd.as_raw_fd(),
        pad: 0,
    };
    unsafe {
        // SAFETY: x is repr(C), x has proper type for the ioctl,
        // and the ioctl does not use the pointer after the call
        ioctl_loop(
            drm_fd,
            DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE,
            &mut x as *mut _ as *mut c_void,
            "fd to handle",
        )?;
        Ok(x.handle)
    }
}

/* To get full IO-safety for DRM handles, would need an OwnedDrmHandle
 * wrapper. As-is, the caller should only call this on syncobj handles
 * it exclusively controls. */
fn drm_syncobj_destroy(drm_fd: &OwnedFd, handle: u32) -> Result<(), String> {
    let mut x = DrmSyncobjDestroy { handle, pad: 0 };
    unsafe {
        // SAFETY: x is repr(C), x has proper type for the ioctl,
        // and the ioctl does not use the pointer after the call
        ioctl_loop(
            drm_fd,
            DRM_IOCTL_SYNCOBJ_DESTROY,
            &mut x as *mut _ as *mut c_void,
            "handle destroy",
        )
    }
}

#[cfg(test)]
pub fn list_vulkan_device_ids() -> Vec<u64> {
    use nix::sys::stat;
    use std::os::unix::ffi::OsStrExt;

    let mut dev_ids = Vec::new();
    let Ok(dir_iter) = std::fs::read_dir("/dev/dri") else {
        /* On failure, assume Vulkan is not available */
        return dev_ids;
    };

    for r in dir_iter {
        let std::io::Result::Ok(entry) = r else {
            continue;
        };
        if !entry.file_name().as_bytes().starts_with(b"renderD") {
            continue;
        }
        let Ok(result) = stat::stat(&entry.path()) else {
            continue;
        };
        dev_ids.push(result.st_rdev);
    }
    dev_ids
}

fn get_max_external_image_size(
    instance: &Instance,
    physdev: vk::PhysicalDevice,
    queue_family: u32,
    format: vk::Format,
    modifier: u64,
    flags: vk::ImageUsageFlags,
) -> Result<(usize, usize), String> {
    let mut ext_create_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let img_qfis = &[queue_family];
    let mut modifier_create_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .queue_family_indices(img_qfis);
    let format_info = vk::PhysicalDeviceImageFormatInfo2KHR::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(flags)
        .flags(vk::ImageCreateFlags::empty())
        .push_next(&mut ext_create_info)
        .push_next(&mut modifier_create_info);

    let mut ext_info = vk::ExternalImageFormatProperties::default();
    let mut image_prop = vk::ImageFormatProperties2::default().push_next(&mut ext_info);
    unsafe {
        instance
            .get_physical_device_image_format_properties2(physdev, &format_info, &mut image_prop)
            .map_err(|x| tag!("Failed to get image format properities: {:?}", x))?;

        Ok((
            image_prop.image_format_properties.max_extent.width as usize,
            image_prop.image_format_properties.max_extent.height as usize,
        ))
    }
}

/* Lower values are assumed better */
fn device_rank(info: &DeviceInfo) -> u8 {
    let base_score = match info.typ {
        vk::PhysicalDeviceType::DISCRETE_GPU => 0,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        vk::PhysicalDeviceType::CPU => 3,
        vk::PhysicalDeviceType::OTHER => 3,
        _ => 3,
    };
    let hw_enc = info.hw_enc_h264;
    let hw_dec = info.hw_dec_h264 | info.hw_dec_av1;
    /* prefers: faster gpu, then hw decoding availability, then hw encoding availability */
    base_score * 4 + ((!hw_enc) as u8) + 2 * ((!hw_dec) as u8)
}

pub struct DeviceInfo {
    physdev: vk::PhysicalDevice,
    device_id: u64,
    typ: vk::PhysicalDeviceType,
    /* If hardware video encoding/decoding is supported; set to false if !with_video */
    pub hw_enc_h264: bool,
    pub hw_dec_h264: bool,
    pub hw_dec_av1: bool,
}

pub fn setup_vulkan(
    main_device: Option<u64>,
    video: &VideoSetting,
    debug: bool,
) -> Result<Arc<Vulkan>, String> {
    let app_name = CString::new(env!("CARGO_PKG_NAME")).unwrap();
    let version: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse::<u32>().unwrap() << 24
        | env!("CARGO_PKG_VERSION_MINOR").parse::<u32>().unwrap() << 16;

    let info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .application_version(version)
        .engine_name(c"waypipe")
        .engine_version(0)
        .api_version(
            if video.dec_pref != Some(CodecPreference::SW)
                || video.enc_pref != Some(CodecPreference::SW)
            {
                // TODO: get best API version available, and turn off video enc/decoding if not?
                vk::make_api_version(0, 1, 3, 0)
            } else {
                vk::make_api_version(0, 1, 0, 0)
            },
        );

    let exts = [
        vk::KHR_GET_PHYSICAL_DEVICE_PROPERTIES2_NAME.as_ptr(), // needed to link device and DRM node
        vk::KHR_EXTERNAL_MEMORY_CAPABILITIES_NAME.as_ptr(),    // needed for buffer import/export
        vk::KHR_EXTERNAL_SEMAPHORE_CAPABILITIES_NAME.as_ptr(), // needed to export/poll on timeline semaphores
    ];

    let ext_list: &[(&CStr, u32)] = &[
        (
            /* Needed to get drm node details */
            vk::EXT_PHYSICAL_DEVICE_DRM_NAME,
            1,
        ),
        (
            /* This and dependencies needed to import/export dmabufs */
            vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME,
            1,
        ),
        (
            /* Used to bind dmabuf memory */
            vk::KHR_BIND_MEMORY2_NAME,
            1,
        ),
        (vk::KHR_SAMPLER_YCBCR_CONVERSION_NAME, 12),
        (vk::KHR_IMAGE_FORMAT_LIST_NAME, 1),
        (vk::KHR_EXTERNAL_MEMORY_NAME, 1),
        (vk::EXT_EXTERNAL_MEMORY_DMA_BUF_NAME, 1),
        (vk::KHR_GET_MEMORY_REQUIREMENTS2_NAME, 1),
        (vk::KHR_EXTERNAL_MEMORY_FD_NAME, 1),
        (vk::KHR_DEDICATED_ALLOCATION_NAME, 3),
        (vk::KHR_MAINTENANCE1_NAME, 1),
        /* For synchronization with the client/compositor, which need
         * not be Vulkan programs themselves */
        (vk::EXT_QUEUE_FAMILY_FOREIGN_NAME, 1),
        /* For timeline semaphores and explicit sync */
        (vk::KHR_TIMELINE_SEMAPHORE_NAME, 2),
        /* To import/export semaphores to fds, for explicit sync protocols & event loop */
        (vk::KHR_EXTERNAL_SEMAPHORE_FD_NAME, 1),
        /* Needed by external_semaphore_fd */
        (vk::KHR_EXTERNAL_SEMAPHORE_NAME, 1),
    ];
    /* Require the latest known version for video related extensions,
     * to be safe, because AVVulkanDeviceContext is only given extension
     * names and not their versions. */
    let ext_list_video_enc_base: &[(&CStr, u32)] = &[(
        vk::KHR_VIDEO_ENCODE_QUEUE_NAME,
        vk::KHR_VIDEO_ENCODE_QUEUE_SPEC_VERSION,
    )];
    let ext_video_enc_h264: (&CStr, u32) = (
        vk::KHR_VIDEO_ENCODE_H264_NAME,
        vk::KHR_VIDEO_ENCODE_H264_SPEC_VERSION,
    );
    let ext_list_video_dec_base: &[(&CStr, u32)] = &[(
        vk::KHR_VIDEO_DECODE_QUEUE_NAME,
        vk::KHR_VIDEO_DECODE_QUEUE_SPEC_VERSION,
    )];
    let ext_video_dec_h264: (&CStr, u32) = (
        vk::KHR_VIDEO_DECODE_H264_NAME,
        vk::KHR_VIDEO_DECODE_H264_SPEC_VERSION,
    );
    let ext_video_dec_av1: (&CStr, u32) = (
        vk::KHR_VIDEO_DECODE_AV1_NAME,
        vk::KHR_VIDEO_DECODE_AV1_SPEC_VERSION,
    );
    let ext_list_video_base: &[(&CStr, u32)] = &[
        (vk::KHR_VIDEO_QUEUE_NAME, vk::KHR_VIDEO_QUEUE_SPEC_VERSION),
        /* Also required, for ffmpeg's vkQueueSubmit2 */
        (
            vk::KHR_SYNCHRONIZATION2_NAME,
            vk::KHR_SYNCHRONIZATION2_SPEC_VERSION,
        ),
        /* YCbCR support */
        (
            vk::KHR_SAMPLER_YCBCR_CONVERSION_NAME,
            vk::KHR_SAMPLER_YCBCR_CONVERSION_SPEC_VERSION,
        ),
        /* For ffmpeg encoding */
        (
            vk::KHR_VIDEO_MAINTENANCE1_NAME,
            vk::KHR_VIDEO_MAINTENANCE1_SPEC_VERSION,
        ),
    ];

    let mut create = vk::InstanceCreateInfo::default()
        .application_info(&info)
        .enabled_extension_names(&exts)
        .flags(vk::InstanceCreateFlags::default());

    let validation = c"VK_LAYER_KHRONOS_validation";
    // let dump = c"VK_LAYER_LUNARG_api_dump";
    let debug_layers = &[validation.as_ptr()];

    unsafe {
        let entry = Entry::load().map_err(|x| tag!("Failed to load Vulkan library: {:?}", x))?;

        if debug {
            /* Only use validation layers with --debug flag if supported */
            let has_validation = entry
                .enumerate_instance_layer_properties()
                .map_err(|x| tag!("Failed to get vulkan layer properties: {:?}", x))?
                .iter()
                .any(|layer| CStr::from_ptr(layer.layer_name.as_ptr()) == validation);
            if has_validation {
                create = create.enabled_layer_names(debug_layers);
            }
        }

        let instance: Instance = entry
            .create_instance(&create, None)
            .map_err(|x| tag!("Failed to create Vulkan instance: {}", x))?;

        /* Note: initial enumeration can be expensive since some details may be loaded
         * even for devices that are not needed */
        let devices = instance
            .enumerate_physical_devices()
            .map_err(|x| tag!("Failed to get physical devices: {:?}", x))?;

        let mut best_device: Option<DeviceInfo> = None;
        for p in devices {
            let exts = instance
                .enumerate_device_extension_properties(p)
                .map_err(|x| tag!("Failed to enumerate device extensions: {:?}", x))?;

            let mut drm_prop = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut prop = vk::PhysicalDeviceProperties2::default();
            let has_drm_name = exts_has_prop(
                &exts,
                vk::EXT_PHYSICAL_DEVICE_DRM_NAME,
                vk::EXT_PHYSICAL_DEVICE_DRM_SPEC_VERSION,
            );
            if has_drm_name {
                prop = prop.push_next(&mut drm_prop);
            }
            instance.get_physical_device_properties2(p, &mut prop);
            let dev_type = prop.properties.device_type;

            debug!(
                "Physical device: {}",
                prop.properties
                    .device_name_as_c_str()
                    .unwrap()
                    .to_str()
                    .unwrap()
            );
            debug!(
                "API {}.{}.{}/{} driver {:#x} vendor {:#x} device {:#x} type {:?}",
                vk::api_version_major(prop.properties.api_version),
                vk::api_version_minor(prop.properties.api_version),
                vk::api_version_patch(prop.properties.api_version),
                vk::api_version_variant(prop.properties.api_version),
                prop.properties.driver_version,
                prop.properties.vendor_id,
                prop.properties.device_id,
                prop.properties.device_type
            );
            if debug {
                if has_drm_name {
                    let primary = if drm_prop.has_primary != 0 {
                        format!("{}.{}", drm_prop.primary_major, drm_prop.primary_minor)
                    } else {
                        String::from("none")
                    };
                    let render = if drm_prop.has_render != 0 {
                        format!("{}.{}", drm_prop.primary_major, drm_prop.primary_minor)
                    } else {
                        String::from("none")
                    };
                    debug!("DRM: primary: {} render: {}", primary, render);
                }

                fn list_missing(
                    specs: &[(&CStr, u32)],
                    exts: &[vk::ExtensionProperties],
                ) -> Vec<String> {
                    specs
                        .iter()
                        .filter_map(|spec| {
                            if !exts_has_prop(exts, spec.0, spec.1) {
                                Some(format!("{}:{}", spec.0.to_str().unwrap(), spec.1))
                            } else {
                                None
                            }
                        })
                        .collect()
                }
                debug!(
                    "Baseline extensions: missing {:?}",
                    list_missing(ext_list, &exts)
                );
                debug!(
                    "Video base extensions: missing {:?}",
                    list_missing(ext_list_video_base, &exts)
                );
                debug!(
                    "Video enc extensions: missing {:?}; has {}, {}",
                    list_missing(ext_list_video_enc_base, &exts),
                    ext_video_enc_h264.0.to_str().unwrap(),
                    fmt_bool(exts_has_prop(
                        &exts,
                        ext_video_enc_h264.0,
                        ext_video_enc_h264.1
                    ))
                );
                debug!(
                    "Video dec extensions: missing {:?}; has {}, {}; has {}, {}",
                    list_missing(ext_list_video_dec_base, &exts),
                    ext_video_dec_h264.0.to_str().unwrap(),
                    fmt_bool(exts_has_prop(
                        &exts,
                        ext_video_dec_h264.0,
                        ext_video_dec_h264.1
                    )),
                    ext_video_dec_av1.0.to_str().unwrap(),
                    fmt_bool(exts_has_prop(
                        &exts,
                        ext_video_dec_av1.0,
                        ext_video_dec_av1.1
                    ))
                );
            }

            let all_present = ext_list
                .iter()
                .all(|(name, version)| exts_has_prop(&exts, name, *version));
            if !all_present {
                continue;
            }

            let mut hw_enc_h264 = false;
            let mut hw_dec_h264 = false;
            let mut hw_dec_av1 = false;

            if video.format.is_some()
                && ext_list_video_base
                    .iter()
                    .all(|(name, version)| exts_has_prop(&exts, name, *version))
            {
                // TODO: first verify that libavcodec has the appropriate encoders/decoders available, if possible
                if video.dec_pref != Some(CodecPreference::SW)
                    && ext_list_video_dec_base
                        .iter()
                        .all(|(name, version)| exts_has_prop(&exts, name, *version))
                {
                    if exts_has_prop(&exts, ext_video_dec_h264.0, ext_video_dec_h264.1) {
                        hw_dec_h264 = true;
                    }
                    if exts_has_prop(&exts, ext_video_dec_av1.0, ext_video_dec_av1.1) {
                        hw_dec_av1 = true;
                    }
                }
                if video.enc_pref != Some(CodecPreference::SW)
                    && ext_list_video_enc_base
                        .iter()
                        .all(|(name, version)| exts_has_prop(&exts, name, *version))
                {
                    if exts_has_prop(&exts, ext_video_enc_h264.0, ext_video_enc_h264.1) {
                        hw_enc_h264 = true;
                    }
                }
            }

            let mut timeline_semaphore_info = vk::SemaphoreTypeCreateInfoKHR::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            /* TODO: should not SYNC_FD be supported and preferable? */
            let ext_semaphore_req = vk::PhysicalDeviceExternalSemaphoreInfo::default()
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                .push_next(&mut timeline_semaphore_info);
            let mut ext_semaphore_info = vk::ExternalSemaphoreProperties::default();
            instance.get_physical_device_external_semaphore_properties(
                p,
                &ext_semaphore_req,
                &mut ext_semaphore_info,
            );
            if !ext_semaphore_info.external_semaphore_features.contains(
                vk::ExternalSemaphoreFeatureFlags::IMPORTABLE_KHR
                    | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE_KHR,
            ) {
                debug!(
                    "Physical device does not support importing or exporting timeline semaphores"
                );
                continue;
            }

            let render_id = if drm_prop.has_render != 0 {
                Some(((drm_prop.render_major as u64) << 8) | (drm_prop.render_minor as u64))
            } else {
                None
            };
            let primary_id = if drm_prop.has_primary != 0 {
                Some(((drm_prop.primary_major as u64) << 8) | (drm_prop.primary_minor as u64))
            } else {
                None
            };

            if let Some(d) = main_device {
                if render_id != Some(d) && primary_id != Some(d) {
                    /* This device corresponds to some other DRM node */
                    continue;
                }
            }

            /* Some device_id is needed for the Wayland application to use */
            let Some(device_id) = render_id.or(primary_id) else {
                continue;
            };

            let proposed = DeviceInfo {
                physdev: p,
                device_id,
                typ: dev_type,
                hw_enc_h264,
                hw_dec_h264,
                hw_dec_av1,
            };
            if let Some(ref cur) = best_device {
                if device_rank(&proposed) < device_rank(cur) {
                    best_device = Some(proposed);
                }
            } else {
                best_device = Some(proposed);
            }
        }

        let Some(dev_info) = best_device else {
            if let Some(d) = main_device {
                return Err(tag!("Failed to find a Vulkan physical device with device id {}, or it does not meet all requirements.", d));
            } else {
                return Err(tag!(
                    "Failed to find any Vulkan physical device meeting all requirements."
                ));
            }
        };
        debug!(
            "Chose physical device with device id: {}",
            dev_info.device_id
        );

        let physdev = dev_info.physdev;
        let using_hw_video = dev_info.hw_enc_h264 | dev_info.hw_dec_h264 | dev_info.hw_dec_av1;

        let memory_properties = instance.get_physical_device_memory_properties(physdev);
        let queue_families = instance.get_physical_device_queue_family_properties(physdev);

        // TODO: use a static (stack) array instead, since max number of extensions is small
        let mut enabled_exts: Vec<*const c_char> = Vec::<*const c_char>::new();
        enabled_exts.extend(ext_list.iter().map(|(name, _)| name.as_ptr()));
        if using_hw_video {
            enabled_exts.extend(ext_list_video_base.iter().map(|(name, _)| name.as_ptr()));
        }
        if dev_info.hw_enc_h264 {
            enabled_exts.extend(
                ext_list_video_enc_base
                    .iter()
                    .map(|(name, _)| name.as_ptr()),
            );
        }
        if dev_info.hw_enc_h264 {
            enabled_exts.push(ext_video_enc_h264.0.as_ptr());
        }
        if dev_info.hw_dec_h264 | dev_info.hw_dec_av1 {
            enabled_exts.extend(
                ext_list_video_dec_base
                    .iter()
                    .map(|(name, _)| name.as_ptr()),
            );
        }
        if dev_info.hw_dec_h264 {
            enabled_exts.push(ext_video_dec_h264.0.as_ptr());
        }
        if dev_info.hw_dec_av1 {
            enabled_exts.push(ext_video_dec_av1.0.as_ptr());
        }

        // order: [compute+transfer, graphics+transfer, encode, decode]
        let qfis = &mut [u32::MAX, u32::MAX, u32::MAX, u32::MAX];
        let nqis = &mut [0, 0, 0, 0];
        for (u, family) in queue_families.iter().enumerate().rev() {
            let i: u32 = u.try_into().unwrap();
            if family
                .queue_flags
                .contains(vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER)
            {
                qfis[0] = i;
                nqis[0] = family.queue_count;
            }
            if family
                .queue_flags
                .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER)
            {
                qfis[1] = i;
                nqis[1] = family.queue_count;
            }
            if family
                .queue_flags
                .contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
            {
                qfis[2] = i;
                nqis[2] = family.queue_count;
            }
            if family
                .queue_flags
                .contains(vk::QueueFlags::VIDEO_DECODE_KHR)
            {
                qfis[3] = i;
                nqis[3] = family.queue_count;
            }
        }

        let queue_family = qfis[0];

        let prio = &[1.0]; // make a single queue
        let cg_queue = qfis[0] == qfis[1];
        let nqf = if using_hw_video {
            if qfis.iter().any(|x| *x == u32::MAX) {
                return Err(tag!("Not all queue types needed available: compute {} graphics {} encode {} decode {}", qfis[0], qfis[1], qfis[2], qfis[3]));
            }

            if cg_queue {
                3
            } else {
                4
            }
        } else {
            1
        };
        let qstart = if cg_queue { 1 } else { 0 };

        let chosen_queues = [
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfis[0])
                .queue_priorities(prio),
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfis[1])
                .queue_priorities(prio),
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfis[2])
                .queue_priorities(prio),
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfis[3])
                .queue_priorities(prio),
        ];

        let mut featuresv11 =
            vk::PhysicalDeviceVulkan11Features::default().sampler_ycbcr_conversion(true);
        let mut featuresv13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
        let mut feature_vid1 =
            vk::PhysicalDeviceVideoMaintenance1FeaturesKHR::default().video_maintenance1(true);
        let mut features2x =
            vk::PhysicalDeviceTimelineSemaphoreFeatures::default().timeline_semaphore(true);
        let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut features2x);
        let mut logical_info = vk::DeviceCreateInfo::default()
            .flags(vk::DeviceCreateFlags::empty())
            .queue_create_infos(&chosen_queues[qstart..qstart + nqf])
            .enabled_extension_names(&enabled_exts)
            .push_next(&mut features2);
        if using_hw_video {
            logical_info = logical_info
                .push_next(&mut featuresv11)
                .push_next(&mut featuresv13)
                .push_next(&mut feature_vid1);
        }

        let dev = match instance.create_device(physdev, &logical_info, None) {
            Ok(x) => x,
            Err(x) => {
                // TODO: cleanup more
                return Err(tag!("Failed to create logical device: {}", x));
            }
        };

        let queue = dev.get_device_queue(queue_family, 0);

        let get_modifier = ext::image_drm_format_modifier::Device::new(&instance, &dev);

        let get_mem_reqs2 = khr::get_memory_requirements2::Device::new(&instance, &dev);
        let bind_mem2 = khr::bind_memory2::Device::new(&instance, &dev);
        let ext_mem_fd = khr::external_memory_fd::Device::new(&instance, &dev);

        let timeline_semaphore = khr::timeline_semaphore::Device::new(&instance, &dev);
        let ext_semaphore_fd = khr::external_semaphore_fd::Device::new(&instance, &dev);

        let mut formats = BTreeMap::<vk::Format, FormatData>::new();
        for f in SUPPORTED_FORMAT_LIST {
            // note: VkFormatProperties3KHR exists, providing more bits for flags
            let mut format_drm_props = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut format_props =
                vk::FormatProperties2::default().push_next(&mut format_drm_props);
            instance.get_physical_device_format_properties2(physdev, *f, &mut format_props);

            if format_drm_props.drm_format_modifier_count == 0 {
                /* No associated modifiers / format not supported for import/export */
                continue;
            }
            let mut dst = Vec::new();
            dst.resize_with(format_drm_props.drm_format_modifier_count as usize, || {
                vk::DrmFormatModifierPropertiesEXT::default()
            });
            format_drm_props = format_drm_props.drm_format_modifier_properties(&mut dst);
            let mut format_props =
                vk::FormatProperties2::default().push_next(&mut format_drm_props);
            instance.get_physical_device_format_properties2(physdev, *f, &mut format_props);

            let info = get_vulkan_info(*f);

            let mut mod_list = Vec::<ModifierData>::new();

            for m in dst.iter() {
                /* YUV formats are only fully supported if one can create and import disjoint planes */
                if info.planes > 1
                    && !m
                        .drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::DISJOINT)
                {
                    continue;
                }

                let base_feature =
                    vk::FormatFeatureFlags::TRANSFER_SRC | vk::FormatFeatureFlags::TRANSFER_DST;
                let base_usage =
                    vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST;
                if !m.drm_format_modifier_tiling_features.contains(base_feature) {
                    continue;
                }

                let max_size_transfer = get_max_external_image_size(
                    &instance,
                    physdev,
                    queue_family,
                    *f,
                    m.drm_format_modifier,
                    base_usage,
                )?;

                let store_feature = vk::FormatFeatureFlags::TRANSFER_SRC
                    | vk::FormatFeatureFlags::TRANSFER_DST
                    | vk::FormatFeatureFlags::STORAGE_IMAGE
                    | vk::FormatFeatureFlags::SAMPLED_IMAGE;
                let store_usage = vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::STORAGE
                    | vk::ImageUsageFlags::SAMPLED;

                let max_size_store_and_sample = if m
                    .drm_format_modifier_tiling_features
                    .contains(store_feature)
                {
                    Some(get_max_external_image_size(
                        &instance,
                        physdev,
                        queue_family,
                        *f,
                        m.drm_format_modifier,
                        store_usage,
                    )?)
                } else {
                    None
                };

                mod_list.push(ModifierData {
                    modifier: m.drm_format_modifier,
                    plane_count: m.drm_format_modifier_plane_count,
                    max_size_transfer,
                    max_size_store_and_sample,
                });
            }

            if video.format.is_some() {
                // todo: only restrict modifiers when the format is usable for video?
                // (in general, if a format supports video encoding, clients should preferably use it.)
                // Alternatively, a preference for video-encodable formats could be made part of dmabuf-feedback,
                // and/or an intermediate storage image could be added to allow video encoding for modifiers
                // which do no support storage or only support small sizes
                if mod_list
                    .iter()
                    .any(|m| m.max_size_store_and_sample.is_some())
                {
                    mod_list.retain(|m| m.max_size_store_and_sample.is_some());
                }
            }

            if mod_list.is_empty() {
                continue;
            }

            formats.insert(
                *f,
                FormatData {
                    modifiers: mod_list,
                },
            );
        }

        let init_sem_value = 0;
        let drm_fd = drm_open_render((dev_info.device_id & 0xff) as u32)?;
        let (semaphore, semaphore_drm_handle, semaphore_fd, semaphore_event_fd) =
            vulkan_create_timeline_parts(&dev, &ext_semaphore_fd, &drm_fd, init_sem_value)?;

        // TODO: if video loading fails, post an error here immediately, for now
        #[cfg(feature = "video")]
        let video = if video.format.is_some() {
            Some(setup_video(
                &entry,
                &instance,
                &physdev,
                &dev,
                &dev_info,
                debug,
                *qfis,
                &enabled_exts,
                &exts,
            )?)
        } else {
            None
        };

        Ok(Arc::new(Vulkan {
            _entry: entry,
            instance,
            _physdev: physdev,
            queue: Mutex::new(VulkanQueue {
                queue,
                last_semaphore_value: init_sem_value,
            }),
            #[cfg(feature = "video")]
            video,
            dev,
            drm_fd,
            semaphore,
            _semaphore_fd: semaphore_fd,
            semaphore_drm_handle,
            event_fd: semaphore_event_fd,
            get_modifier,
            get_mem_reqs2,
            bind_mem2,
            ext_mem_fd,
            memory_properties,
            timeline_semaphore,
            ext_semaphore_fd,
            device_id: dev_info.device_id,
            formats,
            queue_family,
        }))
    }
}

fn vulkan_get_memory_type_index(
    info: &Vulkan,
    bitmask: u32,
    flags: vk::MemoryPropertyFlags,
) -> u32 {
    for (i, t) in info.memory_properties.memory_types
        [..(info.memory_properties.memory_type_count as usize)]
        .iter()
        .enumerate()
    {
        if t.property_flags.contains(flags) && (bitmask & (1u32 << i)) != 0 {
            return i as u32;
        }
    }
    panic!("No matching memory type for {:?}", flags);
}

/** Image memory barrier for use when transferring image to the current queue from the FOREIGN queue.
 *
 * The access range is COLOR for the single level/layer of the entire image. */
pub fn qfot_acquire_image_memory_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    queue_family_idx: u32,
    dst_access_mask: vk::AccessFlags,
) -> vk::ImageMemoryBarrier<'static> {
    let standard_access_range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);

    vk::ImageMemoryBarrier::default()
        .image(image)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
        .dst_queue_family_index(queue_family_idx)
        /* queue family transfer acquire = srcAccessMask ignored, zero value recommended */
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(dst_access_mask)
        .subresource_range(standard_access_range)
}

/** Image memory barrier for use when transferring image from the current queue to the FOREIGN queue.
 *
 * The access range is COLOR for the single level/layer of the entire image. */
pub fn qfot_release_image_memory_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    queue_family_idx: u32,
    src_access_mask: vk::AccessFlags,
) -> vk::ImageMemoryBarrier<'static> {
    let standard_access_range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);

    vk::ImageMemoryBarrier::default()
        .image(image)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(queue_family_idx)
        .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
        /* queue family transfer release = dstAccessMask ignored, zero value recommended */
        .src_access_mask(src_access_mask)
        .dst_access_mask(vk::AccessFlags::empty())
        .subresource_range(standard_access_range)
}

fn memory_plane(x: usize) -> vk::ImageAspectFlags {
    match x {
        0 => vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        1 => vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
        2 => vk::ImageAspectFlags::MEMORY_PLANE_2_EXT,
        3 => vk::ImageAspectFlags::MEMORY_PLANE_3_EXT,
        _ => panic!("Out of bounds"),
    }
}

fn create_cpu_visible_buffer(
    vulk: &Vulkan,
    size: usize,
    read_optimized: bool,
) -> Result<(vk::Buffer, vk::DeviceMemory, u64), String> {
    let buf_create = vk::BufferCreateInfo::default()
        .size(size as u64)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .flags(vk::BufferCreateFlags::empty())
        .usage(
            vk::BufferUsageFlags::TRANSFER_SRC
                | vk::BufferUsageFlags::TRANSFER_DST
                | vk::BufferUsageFlags::STORAGE_TEXEL_BUFFER
                | vk::BufferUsageFlags::UNIFORM_TEXEL_BUFFER,
        );

    unsafe {
        let buffer = vulk
            .dev
            .create_buffer(&buf_create, None)
            .map_err(|_| "Failed to create buffer")?;

        let memreq = vulk.dev.get_buffer_memory_requirements(buffer);
        assert!(memreq.size >= size as u64);

        /* note: not HOST_COHERENT, so memory must be explicitly flushed or invalidated */
        let mem_type = if read_optimized {
            vk::MemoryPropertyFlags::HOST_VISIBLE |
            /* HOST_CACHED allows for much faster processing from CPU side; without
             * it, individual read movs can become very slow. Probably not important
             * for writing due to sequential pattern + write combination. */
            vk::MemoryPropertyFlags::HOST_CACHED
        } else {
            vk::MemoryPropertyFlags::HOST_VISIBLE
        };

        let mem_index = vulkan_get_memory_type_index(vulk, memreq.memory_type_bits, mem_type);

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(memreq.size)
            .memory_type_index(mem_index);

        /* note: allocate_memory output will at least as aligned as memreq.alignment */
        let mem = vulk
            .dev
            .allocate_memory(&alloc_info, None)
            .map_err(|_| "Failed to allocate memory for buffer")?;

        vulk.dev
            .bind_buffer_memory(buffer, mem, 0)
            .map_err(|_| "Failed to bind memory to buffer")?;

        Ok((buffer, mem, memreq.size))
    }
}

pub fn vulkan_get_buffer(
    vulk: &Arc<Vulkan>,
    nom_len: usize,
    read_optimized: bool,
) -> Result<VulkanBuffer, String> {
    let (buffer, mem, len) = create_cpu_visible_buffer(vulk, nom_len, read_optimized)?;

    unsafe {
        let data: *mut c_void = vulk
            .dev
            .map_memory(mem, 0, len, vk::MemoryMapFlags::empty())
            .map_err(|_| "Failed to map memory")?;
        // TODO: proper error handling; need to clean everything up

        Ok(VulkanBuffer {
            vulk: vulk.clone(),
            buffer,
            mem,
            memory_len: len,
            buffer_len: nom_len as u64,
            inner: Mutex::new(VulkanBufferInner {
                data,
                reader_count: 0,
                has_writer: false,
            }),
        })
    }
}

pub fn vulkan_get_cmd_pool(vulk: &Arc<Vulkan>) -> Result<Arc<VulkanCommandPool>, String> {
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vulk.queue_family)
        .flags(vk::CommandPoolCreateFlags::empty()); // todo: transient? reset?

    let pool = unsafe {
        vulk.dev
            .create_command_pool(&pool_info, None)
            .map_err(|_| "Failed to create command pool")?
    };
    Ok(Arc::new(VulkanCommandPool {
        vulk: vulk.clone(),
        pool: Mutex::new(pool),
    }))
}

pub struct VulkanBufferReadView<'a> {
    buffer: &'a VulkanBuffer,
    pub data: &'a [u8],
}

pub struct VulkanBufferWriteView<'a> {
    buffer: &'a VulkanBuffer,
    pub data: &'a mut [u8],
}

impl VulkanBuffer {
    pub fn prepare_read(self: &VulkanBuffer) -> Result<(), String> {
        unsafe {
            let ranges = &[vk::MappedMemoryRange::default()
                .offset(0)
                .size(self.memory_len)
                .memory(self.mem)];

            self.vulk
                .dev
                .invalidate_mapped_memory_ranges(ranges)
                .map_err(|_| "Failed to invalidate mapped memory range")?;
        }
        Ok(())
    }

    pub fn complete_write(self: &VulkanBuffer) -> Result<(), String> {
        unsafe {
            let ranges = &[vk::MappedMemoryRange::default()
                .offset(0)
                .size(self.memory_len)
                .memory(self.mem)];

            self.vulk
                .dev
                .flush_mapped_memory_ranges(ranges)
                .map_err(|_| "Failed to invalidate mapped memory range")?;
        }
        Ok(())
    }

    pub fn get_read_view(self: &VulkanBuffer) -> VulkanBufferReadView {
        let mut inner = self.inner.lock().unwrap();
        let dst = slice_from_raw_parts(inner.data as *const u8, self.buffer_len as usize);
        assert!(!inner.has_writer);
        inner.reader_count += 1;
        unsafe {
            // SAFETY: todo
            VulkanBufferReadView {
                buffer: self,
                data: &*dst,
            }
        }
    }

    pub fn get_write_view(self: &VulkanBuffer) -> VulkanBufferWriteView {
        let mut inner = self.inner.lock().unwrap();
        let dst = slice_from_raw_parts_mut(inner.data as *mut u8, self.buffer_len as usize);
        assert!(inner.reader_count == 0);
        inner.has_writer = true;
        unsafe {
            // SAFETY: todo
            VulkanBufferWriteView {
                buffer: self,
                data: &mut *dst,
            }
        }
    }
}

impl Drop for VulkanBufferReadView<'_> {
    fn drop(&mut self) {
        self.buffer.inner.lock().unwrap().reader_count -= 1;
    }
}
impl Drop for VulkanBufferWriteView<'_> {
    fn drop(&mut self) {
        self.buffer.inner.lock().unwrap().has_writer = false;
    }
}

pub fn vulkan_import_dmabuf(
    vulk: &Arc<Vulkan>,
    planes: Vec<AddDmabufPlane>, // takes ownership, consumes fd. TODO: proper cleanup if this fails early
    width: u32,
    height: u32,
    drm_format: u32,
    can_store_and_sample: bool,
) -> Result<Arc<VulkanDmabuf>, String> {
    let vk_format = drm_to_vulkan(drm_format)
        .ok_or_else(|| tag!("Did not find matching Vulkan format for {}", drm_format))?;
    let format_info = get_vulkan_info(vk_format);

    // note: we should still handle planes together, if only because it will be
    // needed for video work
    let mut layout = Vec::new(); // todo: any 'size-limited-vec-from-array' type to use?

    let mut plane_perm: Vec<usize> = (0..planes.len()).collect();
    plane_perm.sort_by_key(|i| planes[*i].plane_idx);

    let modifier = planes[0].modifier;
    /* post linux-dmabuf version 5, all planes must have same modifier */
    assert!(planes.iter().all(|x| x.modifier == modifier));
    /* Check that all planes are represented */
    assert!(plane_perm
        .iter()
        .enumerate()
        .all(|(i, x)| planes[*x].plane_idx == i as u32));

    let mod_data = vulk.formats[&vk_format]
        .modifiers
        .iter()
        .find(|x| x.modifier == modifier)
        .unwrap();
    let max_size = if can_store_and_sample {
        mod_data.max_size_store_and_sample.unwrap()
    } else {
        mod_data.max_size_transfer
    };
    assert!(width as usize <= max_size.0 && height as usize <= max_size.1);

    // todo: invert?
    for j in plane_perm.iter() {
        layout.push(vk::SubresourceLayout {
            offset: planes[*j].offset as u64,
            row_pitch: planes[*j].stride as u64,
            size: 0,        /* required to be zero per VK_EXT_image_drm_format_modifier */
            array_pitch: 0, /* not relevant for 2d image */
            depth_pitch: 0, /* not relevant for 2d image */
        });
    }

    let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .plane_layouts(&layout)
        .drm_format_modifier(modifier);

    let mut ext_create_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

    let init_layout = vk::ImageLayout::UNDEFINED;

    let usage_bits = if can_store_and_sample {
        vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::STORAGE
            | vk::ImageUsageFlags::SAMPLED
    } else {
        vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST
    };

    let image_info = vk::ImageCreateInfo::default()
        .flags(if format_info.planes > 1 {
            vk::ImageCreateFlags::DISJOINT
        } else {
            vk::ImageCreateFlags::empty()
        })
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk_format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage_bits)
        .sharing_mode(vk::SharingMode::EXCLUSIVE) // only one queue family may use this
        .initial_layout(init_layout)
        .push_next(&mut ext_create_info)
        .push_next(&mut modifier_info);

    unsafe {
        let image = vulk.dev.create_image(&image_info, None).map_err(|x| {
            tag!(
                "Failed to create Vulkan image when importing dmabuf: {:?}",
                x
            )
        })?;

        if format_info.planes > 1 {
            /* Unclear how to handle hypothetical multi-planar formats with metadata memory planes */
            assert!(planes.len() == format_info.planes);
        }

        // TODO: handle non-disjoint YUV
        let nbindplanes = format_info.planes;

        let fds: Vec<OwnedFd> = planes.into_iter().map(|x| x.fd).take(nbindplanes).collect();

        let mut bind_planes: Vec<vk::BindImagePlaneMemoryInfo> = (0..nbindplanes)
            .map(|i| vk::BindImagePlaneMemoryInfo::default().plane_aspect(memory_plane(i)))
            .collect();

        let mut bind_infos: Vec<vk::BindImageMemoryInfo<'_>> = Vec::new();
        for ((plane, fd), bind_plane) in fds.into_iter().enumerate().zip(bind_planes.iter_mut()) {
            let mut mem_props = vk::MemoryFdPropertiesKHR::default();
            if let Err(x) = vulk.ext_mem_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut mem_props,
            ) {
                for b in bind_infos {
                    vulk.dev.free_memory(b.memory, None);
                }
                vulk.dev.destroy_image(image, None);
                return Err(tag!(
                    "Failed to get memory fd properties for plane {}: {}",
                    plane,
                    x
                ));
            };

            let plane_aspect = memory_plane(plane);

            /* Needed for allocation size */
            let mut req_plane_info =
                vk::ImagePlaneMemoryRequirementsInfo::default().plane_aspect(plane_aspect);
            let mut req_info = vk::ImageMemoryRequirementsInfo2::default().image(image);
            if nbindplanes > 1 {
                req_info = req_info.push_next(&mut req_plane_info);
            }
            let mut req_out = vk::MemoryRequirements2::default();
            vulk.get_mem_reqs2
                .get_image_memory_requirements2(&req_info, &mut req_out);

            let mem_candidates =
                mem_props.memory_type_bits & req_out.memory_requirements.memory_type_bits;
            assert!(mem_candidates != 0);
            let mem_index = mem_candidates.trailing_zeros();

            // TODO: importing transfers the file descriptor!
            let mut import_info = vk::ImportMemoryFdInfoKHR::default()
                .fd(fd.into_raw_fd())
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

            let mut dedicate_info = vk::MemoryDedicatedAllocateInfo::default().image(image);

            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(req_out.memory_requirements.size)
                .memory_type_index(mem_index)
                .push_next(&mut import_info)
                .push_next(&mut dedicate_info);

            let mem = match vulk.dev.allocate_memory(&alloc_info, None) {
                Ok(x) => x,
                Err(x) => {
                    for b in bind_infos {
                        vulk.dev.free_memory(b.memory, None);
                    }
                    vulk.dev.destroy_image(image, None);
                    return Err(tag!("Failed to allocate memory: {}", x));
                }
            };

            let bind_img = vk::BindImageMemoryInfo::default()
                .image(image)
                .memory(mem)
                .memory_offset(0); /* memory offset must be zero unless aliasing */

            bind_infos.push(if nbindplanes > 1 {
                bind_img.push_next(bind_plane)
            } else {
                bind_img
            });
        }

        if let Err(x) = (vulk.bind_mem2.fp().bind_image_memory2_khr)(
            vulk.bind_mem2.device(),
            bind_infos.len().try_into().unwrap(),
            bind_infos.as_ptr(),
        )
        .result()
        {
            for b in bind_infos {
                vulk.dev.free_memory(b.memory, None);
            }
            vulk.dev.destroy_image(image, None);
            return Err(tag!("Failed to bind memory: {}", x));
        }

        // Keep VkDeviceMemory around, to free properly later
        let mut mem_planes: Vec<(vk::DeviceMemory, u32, u32)> = Vec::new();
        for i in 0..bind_infos.len() {
            mem_planes.push((
                bind_infos[i].memory,
                layout[i].offset.try_into().unwrap(),
                layout[i].row_pitch.try_into().unwrap(),
            ));
        }

        Ok(Arc::new(VulkanDmabuf {
            vulk: vulk.clone(),
            image,
            inner: Mutex::new(VulkanDmabufInner {
                image_layout: init_layout,
            }),
            memory_planes: mem_planes,
            vk_format,
            can_store_and_sample,
            width: width as usize,
            height: height as usize,
        }))
    }
}

/* Note: the planes do _not_ need to match the original's dimensions */
pub fn vulkan_create_dmabuf(
    vulk: &Arc<Vulkan>,
    width: u32,
    height: u32,
    drm_format: u32,
    modifier_options: &[u64],
    can_store_and_sample: bool,
) -> Result<(Arc<VulkanDmabuf>, Vec<AddDmabufPlane>), String> {
    let vk_format = drm_to_vulkan(drm_format)
        .ok_or_else(|| tag!("Did not find matching Vulkan format for {}", drm_format))?;
    let format_info = get_vulkan_info(vk_format);

    let format_data = &vulk.formats[&vk_format];

    /*<- the list of modifiers that may be chosen */
    let mut mod_options = Vec::new();
    for v in format_data.modifiers.iter() {
        if !modifier_options.contains(&v.modifier) {
            continue;
        }

        let max_size = if can_store_and_sample {
            let Some(s) = v.max_size_store_and_sample else {
                continue;
            };
            s
        } else {
            v.max_size_transfer
        };
        if width as usize > max_size.0 || height as usize > max_size.1 {
            continue;
        }
        mod_options.push(v.modifier);
    }
    if mod_options.is_empty() {
        return Err(tag!(
            "No available modifiers for image with format {}, size {}x{}, store+sample={}",
            drm_format,
            width,
            height,
            can_store_and_sample
        ));
    }

    unsafe {
        let nplanes = format_info.planes;

        let init_layout = vk::ImageLayout::UNDEFINED;

        let mut modifier_info = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&mod_options);

        let mut ext_create_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let usage_bits = if can_store_and_sample {
            vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::STORAGE
                | vk::ImageUsageFlags::SAMPLED
        } else {
            vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST
        };

        let image_info = vk::ImageCreateInfo::default()
            .flags(if format_info.planes > 1 {
                vk::ImageCreateFlags::DISJOINT
            } else {
                vk::ImageCreateFlags::empty()
            })
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage_bits)
            .sharing_mode(vk::SharingMode::EXCLUSIVE) // only one queue family may use this
            .initial_layout(init_layout)
            .push_next(&mut ext_create_info)
            .push_next(&mut modifier_info);

        let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();

        let image = vulk
            .dev
            .create_image(&image_info, None)
            .map_err(|_| "Failed to create image")?;

        if let Err(x) = vulk
            .get_modifier
            .get_image_drm_format_modifier_properties(image, &mut props)
        {
            vulk.dev.destroy_image(image, None);
            return Err(tag!("Failed to get image format modifiers: {}", x));
        }

        let mod_info = format_data
            .modifiers
            .iter()
            .find(|x| x.modifier == props.drm_format_modifier)
            .unwrap();
        let nmemoryplanes = mod_info.plane_count as usize;

        let mut bind_infos: Vec<vk::BindImageMemoryInfoKHR<'_>> = Vec::new(); // todo: fixed size array
        let mut planes = Vec::<AddDmabufPlane>::new();
        let mut mem_fds = Vec::new();
        for plane in 0..nplanes {
            let plane_aspect = memory_plane(plane);
            let mut req_plane_info =
                vk::ImagePlaneMemoryRequirementsInfo::default().plane_aspect(plane_aspect);
            let mut req_info = vk::ImageMemoryRequirementsInfo2::default().image(image);
            if nplanes > 1 {
                req_info = req_info.push_next(&mut req_plane_info);
            }

            let mut req_out = vk::MemoryRequirements2::default();

            // TODO: load via KHR instead?
            vulk.get_mem_reqs2
                .get_image_memory_requirements2(&req_info, &mut req_out);

            assert!(req_out.memory_requirements.memory_type_bits != 0);
            // Pick first/fastest type which is acceptable
            let mem_index = req_out
                .memory_requirements
                .memory_type_bits
                .trailing_zeros();

            let mut export_info = vk::ExportMemoryAllocateInfoKHR::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicate_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(req_out.memory_requirements.size)
                .memory_type_index(mem_index)
                .push_next(&mut dedicate_info)
                .push_next(&mut export_info);
            let mem = match vulk.dev.allocate_memory(&alloc_info, None) {
                Ok(x) => x,
                Err(x) => {
                    for b in bind_infos {
                        vulk.dev.free_memory(b.memory, None);
                    }
                    vulk.dev.destroy_image(image, None);
                    return Err(tag!("Failed to allocate memory: {}", x));
                }
            };

            bind_infos.push(
                vk::BindImageMemoryInfo::default()
                    .image(image)
                    .memory(mem)
                    .memory_offset(0),
            );

            let memory_fd_get_info = vk::MemoryGetFdInfoKHR::default()
                .memory(mem)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

            let fd = vulk
                .ext_mem_fd
                .get_memory_fd(&memory_fd_get_info)
                .map_err(|_| "Failed to get memory fd")?;
            // SAFETY: fd only captured here and vkGetMemoryFdKHR transfers ownership
            mem_fds.push(OwnedFd::from_raw_fd(fd));
        }

        if mem_fds.len() != nmemoryplanes {
            assert!(mem_fds.len() == 1);
            /* duplicate fd for each memory plane */
            let fd = mem_fds.pop().unwrap();
            mem_fds.resize_with(nmemoryplanes - 1, || fd.try_clone().unwrap());
            mem_fds.push(fd);
        }

        for (plane, fd) in std::iter::zip(0..nmemoryplanes, mem_fds) {
            let plane_aspect = memory_plane(plane);

            let layout = vulk.dev.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default()
                    .mip_level(0)
                    .array_layer(0)
                    .aspect_mask(plane_aspect),
            );

            planes.push(AddDmabufPlane {
                fd,
                plane_idx: plane as u32,
                offset: layout.offset.try_into().unwrap(),
                stride: layout.row_pitch.try_into().unwrap(),
                modifier: props.drm_format_modifier,
            });
        }

        if let Err(x) = (vulk.bind_mem2.fp().bind_image_memory2_khr)(
            vulk.bind_mem2.device(),
            bind_infos.len().try_into().unwrap(),
            bind_infos.as_ptr(),
        )
        .result()
        {
            for b in bind_infos {
                vulk.dev.free_memory(b.memory, None);
            }
            vulk.dev.destroy_image(image, None);
            return Err(tag!("Failed to bind memory: {}", x));
        }

        let mut mem_planes: Vec<(vk::DeviceMemory, u32, u32)> = Vec::new();
        for i in 0..bind_infos.len() {
            mem_planes.push((bind_infos[i].memory, planes[i].offset, planes[i].stride));
        }

        Ok((
            Arc::new(VulkanDmabuf {
                vulk: vulk.clone(),
                image,
                inner: Mutex::new(VulkanDmabufInner {
                    image_layout: init_layout,
                }),
                memory_planes: mem_planes,
                vk_format,
                can_store_and_sample,
                width: width as usize,
                height: height as usize,
            }),
            planes,
        ))
    }
}

fn make_evt_fd() -> Result<OwnedFd, String> {
    unsafe {
        let event_init: c_uint = 0;
        // EFD_SEMAPHORE ??
        let ev_flags: c_int = nix::libc::EFD_CLOEXEC | nix::libc::EFD_NONBLOCK;
        let ev_fd: i32 = nix::libc::eventfd(event_init, ev_flags);
        if ev_fd == -1 {
            return Err(tag!("Failed to create eventfd: {}", errno::Errno::last()));
        }
        // SAFETY: ev_fd only captured here and was checked valid
        let event_fd = OwnedFd::from_raw_fd(ev_fd);
        Ok(event_fd)
    }
}

pub fn vulkan_import_timeline(
    vulk: &Arc<Vulkan>,
    fd: OwnedFd,
) -> Result<Arc<VulkanTimelineSemaphore>, String> {
    let mut sem_exp_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
    let mut sem_type = vk::SemaphoreTypeCreateInfoKHR::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE_KHR)
        .initial_value(0);
    let create_semaphore_info = vk::SemaphoreCreateInfo::default()
        .flags(vk::SemaphoreCreateFlags::empty())
        .push_next(&mut sem_type)
        .push_next(&mut sem_exp_info);

    unsafe {
        let semaphore_drm_handle = drm_syncobj_fd_to_handle(&vulk.drm_fd, &fd)?;

        /* This semaphore's contents will be _replaced_ by the import */
        let semaphore = match vulk.dev.create_semaphore(&create_semaphore_info, None) {
            Ok(x) => x,
            Err(_) => {
                drm_syncobj_destroy(&vulk.drm_fd, semaphore_drm_handle).unwrap();
                return Err(tag!("Failed to create semaphore"));
            }
        };

        let raw_fd = fd.into_raw_fd(); // TODO: if import or any following step fails, need to free.
        let import = vk::ImportSemaphoreFdInfoKHR::default()
            .fd(raw_fd)
            .flags(vk::SemaphoreImportFlags::empty())
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
            .semaphore(semaphore);

        match vulk.ext_semaphore_fd.import_semaphore_fd(&import) {
            Ok(()) => (),
            Err(_) => {
                /* Import failed, must clean up fd */
                nix::unistd::close(raw_fd).unwrap();
                vulk.dev.destroy_semaphore(semaphore, None);
                drm_syncobj_destroy(&vulk.drm_fd, semaphore_drm_handle).unwrap();
                return Err(tag!("Failed to import semaphore"));
            }
        };

        let event_fd = match make_evt_fd() {
            Ok(x) => x,
            Err(y) => {
                vulk.dev.destroy_semaphore(semaphore, None);
                drm_syncobj_destroy(&vulk.drm_fd, semaphore_drm_handle).unwrap();
                return Err(y);
            }
        };

        Ok(Arc::new(VulkanTimelineSemaphore {
            vulk: vulk.clone(),
            semaphore,
            semaphore_drm_handle,
            event_fd,
        }))
    }
}
/* Pass in individual structures, since these may be created on setup */
unsafe fn vulkan_create_timeline_parts(
    dev: &Device,
    ext_semaphore_fd: &khr::external_semaphore_fd::Device,
    drm_fd: &OwnedFd,
    start_pt: u64,
) -> Result<(vk::Semaphore, u32, OwnedFd, OwnedFd), String> {
    let mut sem_exp_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
    let mut sem_type = vk::SemaphoreTypeCreateInfoKHR::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE_KHR)
        .initial_value(start_pt);
    let create_semaphore_info = vk::SemaphoreCreateInfo::default()
        .flags(vk::SemaphoreCreateFlags::empty())
        .push_next(&mut sem_type)
        .push_next(&mut sem_exp_info);

    let semaphore = dev
        .create_semaphore(&create_semaphore_info, None)
        .map_err(|_| "Failed to create semaphore")?;

    let sem_fd_info = vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);

    let semaphore_fd = match ext_semaphore_fd.get_semaphore_fd(&sem_fd_info) {
        Ok(x) => {
            // SAFETY: fd only captured here, vkGetSemaphoreFdKHR transfers ownership
            OwnedFd::from_raw_fd(x)
        }
        Err(_) => {
            dev.destroy_semaphore(semaphore, None);
            return Err(tag!("Failed to export semaphore"));
        }
    };

    let semaphore_drm_handle = match drm_syncobj_fd_to_handle(drm_fd, &semaphore_fd) {
        Ok(handle) => handle,
        Err(x) => {
            /* semaphore_fd automatically destroyed */
            dev.destroy_semaphore(semaphore, None);
            return Err(x);
        }
    };
    let event_fd = match make_evt_fd() {
        Ok(fd) => fd,
        Err(x) => {
            /* semaphore_fd automatically destroyed */
            drm_syncobj_destroy(drm_fd, semaphore_drm_handle).unwrap();
            dev.destroy_semaphore(semaphore, None);
            return Err(x);
        }
    };

    Ok((semaphore, semaphore_drm_handle, semaphore_fd, event_fd))
}

pub fn vulkan_create_timeline(
    vulk: &Arc<Vulkan>,
    start_pt: u64,
) -> Result<(Arc<VulkanTimelineSemaphore>, OwnedFd), String> {
    unsafe {
        let (semaphore, semaphore_drm_handle, semaphore_fd, event_fd) =
            vulkan_create_timeline_parts(
                &vulk.dev,
                &vulk.ext_semaphore_fd,
                &vulk.drm_fd,
                start_pt,
            )?;

        Ok((
            Arc::new(VulkanTimelineSemaphore {
                vulk: vulk.clone(),
                semaphore,
                semaphore_drm_handle,
                event_fd,
            }),
            semaphore_fd,
        ))
    }
}

pub fn start_copy_segments_from_dmabuf(
    img: &Arc<VulkanDmabuf>,
    copy: &Arc<VulkanBuffer>,
    pool: &Arc<VulkanCommandPool>,
    segments: &[(u32, u32, u32)],
    view_row_length: Option<u32>,
    wait_semaphores: &[(Arc<VulkanTimelineSemaphore>, u64)],
) -> Result<VulkanCopyHandle, String> {
    // TODO: validate that buffer/dmabuf regions affected are not being used by any other transfer
    // (otherwise callers risk unsoundness)
    let vulk: &Vulkan = &img.vulk;
    let format_info = get_vulkan_info(img.vk_format);
    // todo: fully synchronous code; even if not high performance enough in practice,
    // will be useful for testing or initial creation

    // TODO: try to have everything necessary allocated in advance / so failures cannot happen

    unsafe {
        /* 1: run and wait for copy command */
        let cmd_pool = pool.pool.lock().unwrap();
        let alloc_cb_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(*cmd_pool)
            .command_buffer_count(1)
            .level(vk::CommandBufferLevel::PRIMARY);

        let cbvec = vulk
            .dev
            .allocate_command_buffers(&alloc_cb_info)
            .map_err(|_| "Failed to allocate command buffers")?;
        drop(cmd_pool);
        let cb = cbvec[0];

        // copy-out target buffer -- store per image? 2x memory is OK; but ultimately this is temporary

        // possible: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT
        let begin_cb_info =
            vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::empty());
        vulk.dev
            .begin_command_buffer(cb, &begin_cb_info)
            .map_err(|_| "Failed to begin command buffer")?;

        let regions = make_copy_regions(segments, format_info, view_row_length, img);

        // TODO: eventually, might copy plane0/plane1/plane2 for multiplanar formats
        let op_layout = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;

        let mut img_inner = img.inner.lock().unwrap();

        let acq_barriers = &[qfot_acquire_image_memory_barrier(
            img.image,
            img_inner.image_layout,
            op_layout,
            vulk.queue_family,
            vk::AccessFlags::TRANSFER_READ,
        )];
        let rel_barriers = &[qfot_release_image_memory_barrier(
            img.image,
            op_layout,
            vk::ImageLayout::GENERAL,
            vulk.queue_family,
            vk::AccessFlags::TRANSFER_READ,
        )];
        let buf_rel_barrier = &[vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .buffer(copy.buffer)
            .offset(0)
            .size(copy.buffer_len)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)];

        // Perform layout transition, even though it is unclear how useful it is for DMABUFs
        vulk.dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            acq_barriers,
        );

        vulk.dev
            .cmd_copy_image_to_buffer(cb, img.image, op_layout, copy.buffer, &regions[..]);

        vulk.dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            buf_rel_barrier,
            &[],
        );
        vulk.dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            rel_barriers,
        );
        img_inner.image_layout = vk::ImageLayout::GENERAL;

        vulk.dev
            .end_command_buffer(cb)
            .map_err(|_| "Failed to end command buffer")?;

        let cbs = &[cb];

        let waitv_values: Vec<u64> = wait_semaphores.iter().map(|x| x.1).collect();
        let waitv_semaphores: Vec<vk::Semaphore> =
            wait_semaphores.iter().map(|x| x.0.semaphore).collect();
        let mut waitv_stage_flags = Vec::new();
        waitv_stage_flags.resize(waitv_semaphores.len(), vk::PipelineStageFlags::ALL_COMMANDS);

        let mut queue = vulkan_lock_queue(vulk);
        queue.inner.last_semaphore_value += 1;
        let completion_time_point = queue.inner.last_semaphore_value;
        let values = &[completion_time_point];
        let semaphores = &[vulk.semaphore];

        let mut signal = vk::TimelineSemaphoreSubmitInfoKHR::default()
            .wait_semaphore_values(&waitv_values[..])
            .signal_semaphore_values(values);

        let submits = &[vk::SubmitInfo::default()
            .command_buffers(cbs)
            .wait_semaphores(&waitv_semaphores[..])
            .wait_dst_stage_mask(&waitv_stage_flags)
            .signal_semaphores(semaphores)
            .push_next(&mut signal)];
        vulk.dev
            .queue_submit(queue.inner.queue, submits, vk::Fence::null())
            .map_err(|_| "Queue submit failed")?; // <- can fail with OOM
        drop(queue);

        Ok(VulkanCopyHandle {
            vulk: img.vulk.clone(),
            _image: img.clone(),
            _buffer: copy.clone(),
            pool: pool.clone(),
            cb,
            completion_time_point,
        })
    }
}

fn make_copy_regions(
    segments: &[(u32, u32, u32)],
    format_info: FormatLayoutInfo,
    view_row_length: Option<u32>,
    img: &VulkanDmabuf,
) -> Vec<vk::BufferImageCopy> {
    let row_length = view_row_length.unwrap_or((img.width * format_info.bpp) as u32);
    assert!(row_length >= (img.width * format_info.bpp) as u32);
    assert!(row_length % (format_info.bpp as u32) == 0);

    let prototype = vk::BufferImageCopy::default()
        .buffer_row_length(row_length / (format_info.bpp as u32))
        .buffer_image_height(0) /* not needed, single layer */
        .image_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .mip_level(0)
                .base_array_layer(0)
                .layer_count(1),
        );
    let z = vk::Offset3D::default();
    let e = vk::Extent3D::default().depth(1);

    // TODO: use an arrayvec equivalent
    let mut regions = Vec::<vk::BufferImageCopy>::new();
    for (mut source_offset, start, end) in segments {
        // TODO: this assumes disjoint segments; validate time?

        // (Can avoid this by super-aligning diffs?)
        let ubpp = format_info.bpp as u32;

        let mut start_row = start / row_length;
        let end_row = (end - 1) / row_length;
        assert!(
            (start % row_length) % ubpp == 0,
            "non-{}-aligned interval [{},{}) with row length {}",
            ubpp,
            start,
            end,
            row_length
        );
        assert!(
            (end % row_length) % ubpp == 0,
            "non-{}-aligned interval [{},{}) with row length {}",
            ubpp,
            start,
            end,
            row_length
        );
        let mut start_pos = (start % row_length) / ubpp;
        let mut end_pos = 1 + ((end - 1) % row_length) / ubpp;
        let w = img.width as u32;
        if start_pos > w {
            /* Advance to next row */
            source_offset += row_length - start_pos * ubpp;
            start_pos = 0;
            start_row += 1;
        }
        if end_pos > w {
            end_pos = w;
        }
        if start_row > end_row || (start_row == end_row && start_pos >= end_pos) {
            /* Can happen if start_pos/end_pos are both in padding region of row */
            continue;
        }

        /* Cases:
         *  x--x |  x-- | x--
         *       | --x  |----
         *       |      |-x
         */
        if start_row == end_row {
            regions.push(
                prototype
                    .buffer_offset(source_offset as u64)
                    .image_offset(z.x(start_pos as i32).y(start_row as i32))
                    .image_extent(e.width(end_pos - start_pos).height(1)),
            );
        } else {
            let (mid_start, mid_row_start): (u32, u32) = if start_pos == 0 {
                (start_row, source_offset)
            } else {
                (
                    start_row + 1,
                    source_offset + (row_length - start_pos * ubpp),
                )
            };
            let mid_end = if end_pos >= w { end_row } else { end_row - 1 };
            assert!(
                mid_end + 1 >= mid_start,
                "{} {} {} {} => {} {}",
                start_pos,
                start_row,
                end_pos,
                end_row,
                mid_end,
                mid_start
            );

            if start_pos > 0 {
                regions.push(
                    prototype
                        .buffer_offset(source_offset as u64)
                        .image_offset(z.x(start_pos as i32).y(start_row as i32))
                        .image_extent(e.width(w - start_pos).height(1)),
                );
            }
            if mid_end >= mid_start {
                regions.push(
                    prototype
                        .buffer_offset(mid_row_start as u64)
                        .image_offset(z.x(0).y(mid_start as i32))
                        .image_extent(e.width(w).height(mid_end - mid_start + 1)),
                );
            }
            if end_pos < w {
                let adv2 = mid_row_start + (row_length * (mid_end - mid_start + 1));
                regions.push(
                    prototype
                        .buffer_offset(adv2 as u64)
                        .image_offset(z.x(0).y(end_row as i32))
                        .image_extent(e.width(end_pos).height(1)),
                );
            }
        }
    }
    regions
}

/* Segment structure: src_start, dst_start, dst_end
 *
 * Cost estimate: 100 segments once took 0.5msec to create and queue, while 1 took 0.1msec
 *
 * Releases: imported timeline semaphores to wait for until buffer is safe to modify
 */
pub fn start_copy_segments_onto_dmabuf(
    img: &Arc<VulkanDmabuf>,
    copy: &Arc<VulkanBuffer>,
    pool: &Arc<VulkanCommandPool>,
    segments: &[(u32, u32, u32)],
    view_row_length: Option<u32>,
    wait_semaphores: &[(Arc<VulkanTimelineSemaphore>, u64)],
) -> Result<VulkanCopyHandle, String> {
    let vulk: &Vulkan = &img.vulk;
    let format_info = get_vulkan_info(img.vk_format);

    // Design: each image gets its own command pool, with four options (diff, fill, etc.)

    unsafe {
        let cmd_pool = pool.pool.lock().unwrap();
        let alloc_cb_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(*cmd_pool)
            .command_buffer_count(1)
            .level(vk::CommandBufferLevel::PRIMARY);

        let cbvec = vulk
            .dev
            .allocate_command_buffers(&alloc_cb_info)
            .map_err(|_| "Failed to allocate command buffers")?;
        drop(cmd_pool);
        let cb = cbvec[0];

        // copy-out target buffer -- store per image? 2x memory is OK; but ultimately this is temporary

        // possible: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT
        let begin_cb_info =
            vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::empty());
        vulk.dev
            .begin_command_buffer(cb, &begin_cb_info)
            .map_err(|_| "Failed to begin command buffer")?;

        let regions = make_copy_regions(segments, format_info, view_row_length, img);

        let mut img_inner = img.inner.lock().unwrap();

        let op_layout = vk::ImageLayout::TRANSFER_DST_OPTIMAL;

        let acq_barriers = &[qfot_acquire_image_memory_barrier(
            img.image,
            img_inner.image_layout,
            op_layout,
            vulk.queue_family,
            vk::AccessFlags::TRANSFER_WRITE,
        )];
        let buf_acq_barrier = &[vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::HOST_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .buffer(copy.buffer)
            .offset(0)
            .size(copy.buffer_len)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)];

        let rel_barriers = &[qfot_release_image_memory_barrier(
            img.image,
            op_layout,
            vk::ImageLayout::GENERAL,
            vulk.queue_family,
            vk::AccessFlags::TRANSFER_WRITE,
        )];

        // Perform layout transition, even though it is unclear how useful it is for DMABUFs
        vulk.dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            acq_barriers,
        );
        vulk.dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            buf_acq_barrier,
            &[],
        );

        vulk.dev
            .cmd_copy_buffer_to_image(cb, copy.buffer, img.image, op_layout, &regions[..]);

        vulk.dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            rel_barriers,
        );
        img_inner.image_layout = vk::ImageLayout::GENERAL;

        vulk.dev
            .end_command_buffer(cb)
            .map_err(|_| "Failed to end command buffer")?;

        let cbs = &[cb];

        let waitv_values: Vec<u64> = wait_semaphores.iter().map(|x| x.1).collect();
        let waitv_semaphores: Vec<vk::Semaphore> =
            wait_semaphores.iter().map(|x| x.0.semaphore).collect();
        let mut waitv_stage_flags = Vec::new();
        waitv_stage_flags.resize(waitv_semaphores.len(), vk::PipelineStageFlags::ALL_COMMANDS);

        let mut queue = vulkan_lock_queue(vulk);
        queue.inner.last_semaphore_value += 1;
        let completion_time_point = queue.inner.last_semaphore_value;
        let values = &[completion_time_point];
        let semaphores = &[vulk.semaphore];
        let mut signal = vk::TimelineSemaphoreSubmitInfoKHR::default()
            .wait_semaphore_values(&waitv_values[..])
            .signal_semaphore_values(values);
        let submits = &[vk::SubmitInfo::default()
            .command_buffers(cbs)
            .wait_semaphores(&waitv_semaphores[..])
            .wait_dst_stage_mask(&waitv_stage_flags[..])
            .signal_semaphores(semaphores)
            .push_next(&mut signal)];
        vulk.dev
            .queue_submit(queue.inner.queue, submits, vk::Fence::null())
            .map_err(|_| "Queue submit failed")?; // <- can fail with OOM
        drop(queue);

        // TODO: clean up or recycle command buffer
        Ok(VulkanCopyHandle {
            vulk: img.vulk.clone(),
            _image: img.clone(),
            _buffer: copy.clone(),
            pool: pool.clone(),
            cb,
            completion_time_point,
        })
    }
}

impl VulkanCopyHandle {
    /* Not recommended in general -- blocks the thread. Returns true if point reached. */
    #[cfg(any(test, feature = "test_proto"))]
    pub fn wait_until_done(self: &VulkanCopyHandle) -> Result<(), String> {
        self.vulk
            .wait_for_timeline_pt(self.completion_time_point, u64::MAX)
            .map(|_| ())
    }
    pub fn get_timeline_point(self: &VulkanCopyHandle) -> u64 {
        self.completion_time_point
    }
}

pub fn get_dev_for_drm_node_path(path: &PathBuf) -> Result<u64, String> {
    let r = nix::sys::stat::stat(path).map_err(|_| "Failed to get device for drm node path")?;
    Ok(r.st_rdev)
}

impl Vulkan {
    /* For indefinite delay, use u64::MAX ~585 years.
     * Returns `true` if wait successful.
     */
    pub fn wait_for_timeline_pt(self: &Vulkan, pt: u64, max_wait: u64) -> Result<bool, String> {
        unsafe {
            let sem = &[self.semaphore];
            let values = &[pt];
            let wait_info = vk::SemaphoreWaitInfoKHR::default()
                .semaphores(sem)
                .values(values)
                .flags(vk::SemaphoreWaitFlags::empty());
            match self
                .timeline_semaphore
                .wait_semaphores(&wait_info, max_wait)
            {
                Ok(()) => Ok(true),
                Err(vk::Result::TIMEOUT) => Ok(false),
                Err(x) => Err(tag!("Waiting for completion failed: {:?}", x)),
            }
        }
    }

    pub fn get_device(self: &Vulkan) -> u64 {
        self.device_id
    }
    pub fn get_event_fd(self: &Vulkan, timeline_point: u64) -> Result<BorrowedFd, String> {
        drm_syncobj_eventfd(
            &self.drm_fd,
            &self.event_fd,
            self.semaphore_drm_handle,
            timeline_point,
        )?;
        Ok(self.event_fd.as_fd())
    }

    pub fn get_current_timeline_pt(self: &Vulkan) -> Result<u64, String> {
        unsafe {
            self.timeline_semaphore
                .get_semaphore_counter_value(self.semaphore)
                .map_err(|x| tag!("Failed to get timeline point: {:?}", x))
        }
    }

    pub fn can_import_image(
        &self,
        drm_format: u32,
        width: u32,
        height: u32,
        planes: &[AddDmabufPlane],
        can_store_and_sample: bool,
    ) -> bool {
        /* post linux-dmabuf version 5, all planes must have same modifier */
        let modifier = planes[0].modifier;
        assert!(planes.iter().all(|x| x.modifier == modifier));

        let Some(vk_format) = drm_to_vulkan(drm_format) else {
            return false;
        };
        let Some(data) = self.formats.get(&vk_format) else {
            return false;
        };
        let Some(mod_data) = data.modifiers.iter().find(|x| x.modifier == modifier) else {
            return false;
        };
        let max_size = if can_store_and_sample {
            mod_data.max_size_store_and_sample.unwrap()
        } else {
            mod_data.max_size_transfer
        };
        width as usize <= max_size.0 && height as usize <= max_size.1
    }

    pub fn supports_format(self: &Vulkan, drm_format: u32, drm_modifier: u64) -> bool {
        let Some(vk_fmt) = drm_to_vulkan(drm_format) else {
            return false;
        };
        let Some(data) = self.formats.get(&vk_fmt) else {
            return false;
        };
        data.modifiers.iter().any(|m| m.modifier == drm_modifier)
    }

    /* Returns empty vector if format is not supported; otherwise a list of permissible modifiers */
    pub fn get_supported_modifiers(self: &Vulkan, drm_format: u32) -> Vec<u64> {
        let Some(vk_fmt) = drm_to_vulkan(drm_format) else {
            return Vec::new();
        };
        let Some(data) = self.formats.get(&vk_fmt) else {
            return Vec::new();
        };
        data.modifiers.iter().map(|m| m.modifier).collect()
    }
}

impl VulkanDmabuf {
    /* The total length of the canonical representation of the format */
    /* If 'view_row_length' is not None, it specifies the row stride to use */
    pub fn nominal_size(self: &VulkanDmabuf, view_row_length: Option<u32>) -> usize {
        let format_info = get_vulkan_info(self.vk_format);
        // TODO: handle multiplanar formats
        if let Some(r) = view_row_length {
            self.height * (r as usize)
        } else {
            self.width * self.height * format_info.bpp
        }
    }
    // todo: will need modification for multi-planar support
    pub fn get_bpp(&self) -> usize {
        let format_info = get_vulkan_info(self.vk_format);
        format_info.bpp
    }

    /* Provide contents of dmabuf_slice_data */
    pub fn ideal_slice_data(self: &VulkanDmabuf, drm_format: u32) -> [u8; 64] {
        let mut out = [0; 64];
        out[0..4].copy_from_slice(&(self.width as u32).to_le_bytes());
        out[4..8].copy_from_slice(&(self.height as u32).to_le_bytes());
        out[8..12].copy_from_slice(&drm_format.to_le_bytes());
        let nmemory_planes = self.memory_planes.len();
        out[12..16].copy_from_slice(&(nmemory_planes as u32).to_le_bytes());

        for i in 0..4 {
            let (offset, stride) = if let Some((_mem, o, s)) = self.memory_planes.get(i) {
                (*o, *s)
            } else {
                (0, 0)
            };
            out[16 + 4 * i..16 + 4 * (i + 1)].copy_from_slice(&offset.to_le_bytes());
            out[32 + 4 * i..32 + 4 * (i + 1)].copy_from_slice(&stride.to_le_bytes());
        }
        let modifier: u64 = 0;
        out[48..56].copy_from_slice(&modifier.to_le_bytes());

        /* Originally used to track which planes correspond to which dmabuf;
         * however, now multiplanar formats are handled as one dmabuf */
        for i in 0..nmemory_planes {
            out[56 + i] = 1;
        }

        out
    }
    pub fn get_first_stride(data: [u8; 64]) -> u32 {
        u32::from_le_bytes(data[32..36].try_into().unwrap())
    }
}

impl VulkanTimelineSemaphore {
    #[allow(dead_code)]
    #[cfg(any(test, feature = "test_proto"))]
    pub fn wait_for_timeline_pt(
        self: &VulkanTimelineSemaphore,
        pt: u64,
        timeout_ns: u64,
    ) -> Result<(), String> {
        unsafe {
            let sem = &[self.semaphore];
            let values = &[pt];
            let wait_info = vk::SemaphoreWaitInfoKHR::default()
                .semaphores(sem)
                .values(values)
                .flags(vk::SemaphoreWaitFlags::empty());
            self.vulk
                .timeline_semaphore
                .wait_semaphores(&wait_info, timeout_ns) // u64::MAX is ~585 years
                .map_err(|x| tag!("Waiting for completion failed: {:?}", x))?;
        }
        Ok(())
    }
    pub fn get_current_pt(self: &VulkanTimelineSemaphore) -> Result<u64, String> {
        unsafe {
            self.vulk
                .timeline_semaphore
                .get_semaphore_counter_value(self.semaphore)
                .map_err(|x| tag!("Failed to get timeline point: {:?}", x))
        }
    }

    pub fn get_event_fd(self: &VulkanTimelineSemaphore) -> BorrowedFd {
        self.event_fd.as_fd()
    }
    pub fn link_event_fd(
        self: &VulkanTimelineSemaphore,
        timeline_point: u64,
    ) -> Result<BorrowedFd, String> {
        drm_syncobj_eventfd(
            &self.vulk.drm_fd,
            &self.event_fd,
            self.semaphore_drm_handle,
            timeline_point,
        )?;
        Ok(self.event_fd.as_fd())
    }
    pub fn signal_timeline_pt(self: &VulkanTimelineSemaphore, pt: u64) -> Result<(), String> {
        unsafe {
            let signal_info = vk::SemaphoreSignalInfo::default()
                .semaphore(self.semaphore)
                .value(pt);
            self.vulk
                .timeline_semaphore
                .signal_semaphore(&signal_info)
                .map_err(|_| tag!("Signalling timeline semaphore failed"))?;
        }
        Ok(())
    }
}

#[allow(dead_code)]
#[cfg(any(test, feature = "test_proto"))]
pub fn copy_onto_dmabuf(
    buf: &Arc<VulkanDmabuf>,
    copy: &Arc<VulkanBuffer>,
    data: &[u8],
) -> Result<(), String> {
    unsafe {
        let nom_len = buf.nominal_size(None);
        // Safety: requires valid region, and no other reader or writer at this time
        assert!(data.len() == nom_len);
        let inner = copy.inner.lock().unwrap();
        let dst = std::ptr::slice_from_raw_parts_mut(inner.data as *mut u8, nom_len);
        (*dst).copy_from_slice(data);

        let ranges = &[vk::MappedMemoryRange::default()
            .offset(0)
            .size(copy.memory_len)
            .memory(copy.mem)];
        let vulk: &Vulkan = &buf.vulk;
        vulk.dev
            .flush_mapped_memory_ranges(ranges)
            .map_err(|_| "Failed to flush mapped memory range")?;

        let pool = vulkan_get_cmd_pool(&buf.vulk)?;
        let handle = start_copy_segments_onto_dmabuf(
            buf,
            copy,
            &pool,
            &[(0, 0, nom_len as u32)],
            None,
            &[],
        )?;
        handle.wait_until_done()?;
        drop(handle);
    }

    Ok(())
}

#[allow(dead_code)]
#[cfg(any(test, feature = "test_proto"))]
pub fn copy_from_dmabuf(
    buf: &Arc<VulkanDmabuf>,
    copy: &Arc<VulkanBuffer>,
) -> Result<Vec<u8>, String> {
    let pool = vulkan_get_cmd_pool(&buf.vulk)?;
    let handle = start_copy_segments_from_dmabuf(
        buf,
        copy,
        &pool,
        &[(0, 0, buf.nominal_size(None) as u32)],
        None,
        &[],
    )?;
    handle.wait_until_done()?;
    drop(handle);

    let nom_len = buf.nominal_size(None);
    let mut output = vec![0; nom_len];

    let vulk: &Vulkan = &buf.vulk;
    unsafe {
        let ranges = &[vk::MappedMemoryRange::default()
            .offset(0)
            .size(copy.memory_len)
            .memory(copy.mem)];
        vulk.dev
            .invalidate_mapped_memory_ranges(ranges)
            .map_err(|_| "Failed to invalidate mapped memory range")?;

        assert!(nom_len as u64 <= copy.memory_len);
        // Safety: requires valid region, and no other writers at this time
        let inner = copy.inner.lock().unwrap();
        let src = slice_from_raw_parts(inner.data as *mut u8, nom_len);
        output.copy_from_slice(&*src);
    }

    Ok(output)
}

#[cfg(test)]
pub const DRM_FORMATS: &[u32] = &[
    fourcc('A', 'R', '2', '4'),
    fourcc('X', 'R', '2', '4'),
    WlShmFormat::C8 as u32,
    WlShmFormat::Rgb332 as u32,
    WlShmFormat::Bgr233 as u32,
    WlShmFormat::Xrgb4444 as u32,
    WlShmFormat::Xbgr4444 as u32,
    WlShmFormat::Rgbx4444 as u32,
    WlShmFormat::Bgrx4444 as u32,
    WlShmFormat::Argb4444 as u32,
    WlShmFormat::Abgr4444 as u32,
    WlShmFormat::Rgba4444 as u32,
    WlShmFormat::Bgra4444 as u32,
    WlShmFormat::Xrgb1555 as u32,
    WlShmFormat::Xbgr1555 as u32,
    WlShmFormat::Rgbx5551 as u32,
    WlShmFormat::Bgrx5551 as u32,
    WlShmFormat::Argb1555 as u32,
    WlShmFormat::Abgr1555 as u32,
    WlShmFormat::Rgba5551 as u32,
    WlShmFormat::Bgra5551 as u32,
    WlShmFormat::Rgb565 as u32,
    WlShmFormat::Bgr565 as u32,
    WlShmFormat::Rgb888 as u32,
    WlShmFormat::Bgr888 as u32,
    WlShmFormat::Xbgr8888 as u32,
    WlShmFormat::Rgbx8888 as u32,
    WlShmFormat::Bgrx8888 as u32,
    WlShmFormat::Abgr8888 as u32,
    WlShmFormat::Rgba8888 as u32,
    WlShmFormat::Bgra8888 as u32,
    WlShmFormat::Xrgb2101010 as u32,
    WlShmFormat::Xbgr2101010 as u32,
    WlShmFormat::Rgbx1010102 as u32,
    WlShmFormat::Bgrx1010102 as u32,
    WlShmFormat::Argb2101010 as u32,
    WlShmFormat::Abgr2101010 as u32,
    WlShmFormat::Rgba1010102 as u32,
    WlShmFormat::Bgra1010102 as u32,
    WlShmFormat::Yuyv as u32,
    WlShmFormat::Yvyu as u32,
    WlShmFormat::Uyvy as u32,
    WlShmFormat::Vyuy as u32,
    WlShmFormat::Ayuv as u32,
    WlShmFormat::Nv12 as u32,
    WlShmFormat::Nv21 as u32,
    WlShmFormat::Nv16 as u32,
    WlShmFormat::Nv61 as u32,
    WlShmFormat::Yuv410 as u32,
    WlShmFormat::Yvu410 as u32,
    WlShmFormat::Yuv411 as u32,
    WlShmFormat::Yvu411 as u32,
    WlShmFormat::Yuv420 as u32,
    WlShmFormat::Yvu420 as u32,
    WlShmFormat::Yuv422 as u32,
    WlShmFormat::Yvu422 as u32,
    WlShmFormat::Yuv444 as u32,
    WlShmFormat::Yvu444 as u32,
    WlShmFormat::R8 as u32,
    WlShmFormat::R16 as u32,
    WlShmFormat::Rg88 as u32,
    WlShmFormat::Gr88 as u32,
    WlShmFormat::Rg1616 as u32,
    WlShmFormat::Gr1616 as u32,
    WlShmFormat::Xrgb16161616f as u32,
    WlShmFormat::Xbgr16161616f as u32,
    WlShmFormat::Argb16161616f as u32,
    WlShmFormat::Abgr16161616f as u32,
    WlShmFormat::Xyuv8888 as u32,
    WlShmFormat::Vuy888 as u32,
    WlShmFormat::Vuy101010 as u32,
    WlShmFormat::Y210 as u32,
    WlShmFormat::Y212 as u32,
    WlShmFormat::Y216 as u32,
    WlShmFormat::Y410 as u32,
    WlShmFormat::Y412 as u32,
    WlShmFormat::Y416 as u32,
    WlShmFormat::Xvyu2101010 as u32,
    WlShmFormat::Xvyu1216161616 as u32,
    WlShmFormat::Xvyu16161616 as u32,
    WlShmFormat::Y0l0 as u32,
    WlShmFormat::X0l0 as u32,
    WlShmFormat::Y0l2 as u32,
    WlShmFormat::X0l2 as u32,
    WlShmFormat::Yuv4208bit as u32,
    WlShmFormat::Yuv42010bit as u32,
    WlShmFormat::Xrgb8888A8 as u32,
    WlShmFormat::Xbgr8888A8 as u32,
    WlShmFormat::Rgbx8888A8 as u32,
    WlShmFormat::Bgrx8888A8 as u32,
    WlShmFormat::Rgb888A8 as u32,
    WlShmFormat::Bgr888A8 as u32,
    WlShmFormat::Rgb565A8 as u32,
    WlShmFormat::Bgr565A8 as u32,
    WlShmFormat::Nv24 as u32,
    WlShmFormat::Nv42 as u32,
    WlShmFormat::P210 as u32,
    WlShmFormat::P010 as u32,
    WlShmFormat::P012 as u32,
    WlShmFormat::P016 as u32,
    WlShmFormat::Axbxgxrx106106106106 as u32,
    WlShmFormat::Nv15 as u32,
    WlShmFormat::Q410 as u32,
    WlShmFormat::Q401 as u32,
    WlShmFormat::Xrgb16161616 as u32,
    WlShmFormat::Xbgr16161616 as u32,
    WlShmFormat::Argb16161616 as u32,
    WlShmFormat::Abgr16161616 as u32,
    WlShmFormat::C1 as u32,
    WlShmFormat::C2 as u32,
    WlShmFormat::C4 as u32,
    WlShmFormat::D1 as u32,
    WlShmFormat::D2 as u32,
    WlShmFormat::D4 as u32,
    WlShmFormat::D8 as u32,
    WlShmFormat::R1 as u32,
    WlShmFormat::R2 as u32,
    WlShmFormat::R4 as u32,
    WlShmFormat::R10 as u32,
    WlShmFormat::R12 as u32,
    WlShmFormat::Avuy8888 as u32,
    WlShmFormat::Xvuy8888 as u32,
    WlShmFormat::P030 as u32,
];

/* Creating a Vulkan instance or connecting ffmpeg to it should be thread safe (safe to
 * run in multiple test threads), but in practice there may be memory corruption visible
 * every few hundred runs. This should not be a problem in practice for Waypipe since
 * setup_vulkan() is only called once; but could make tests flaky. */
#[cfg(test)]
pub static VULKAN_MUTEX: Mutex<()> = Mutex::new(());

#[test]
fn test_dmabuf() {
    let _serialize_test = VULKAN_MUTEX.lock().unwrap();

    for dev_id in list_vulkan_device_ids() {
        let Ok(vulk) = setup_vulkan(Some(dev_id), &VideoSetting::default(), true) else {
            continue;
        };

        println!("Setup complete for device id {}", dev_id);

        let mut format_modifiers = Vec::<(u32, u64)>::new();
        for f in DRM_FORMATS {
            let Some(vkf) = drm_to_vulkan(*f) else {
                continue;
            };
            let Some(data) = vulk.formats.get(&vkf) else {
                continue;
            };
            for m in &data.modifiers {
                format_modifiers.push((*f, m.modifier));
            }
        }

        println!("formats: {:#?}", vulk.formats);

        for (j, (format, modifier)) in format_modifiers.iter().enumerate() {
            let (format, modifier) = (*format, *modifier);
            let vkf = drm_to_vulkan(format).unwrap();
            println!(
                "\nTesting format 0x{:x} => {:?}, modifier 0x{:x}",
                format, vkf, modifier
            );
            let (width, height) = (110, 44);
            let bpp = get_vulkan_info(drm_to_vulkan(format).unwrap()).bpp;

            let start_time = std::time::Instant::now();

            let mod_options = &[modifier];
            let (dmabuf1, planes) =
                vulkan_create_dmabuf(&vulk, width, height, format, mod_options, false).unwrap();

            println!("DMABUF for 0x{:x} created with planes {:?}", format, planes);

            let dmabuf2 =
                vulkan_import_dmabuf(&vulk, planes, width, height, format, false).unwrap();

            println!("DMABUF imported");

            let mut pattern: Vec<u8> = vec![0; (width * height) as usize * bpp];
            for x in pattern.iter_mut().enumerate() {
                *x.1 = (x.0 * (j + 1)) as u8;
            }
            let copy1 =
                Arc::new(vulkan_get_buffer(&vulk, dmabuf1.nominal_size(None), true).unwrap());
            let copy2 =
                Arc::new(vulkan_get_buffer(&vulk, dmabuf2.nominal_size(None), true).unwrap());

            copy_onto_dmabuf(&dmabuf1, &copy1, &pattern[..]).unwrap();
            let output = copy_from_dmabuf(&dmabuf2, &copy2).unwrap();

            let end_time = std::time::Instant::now();
            let duration = end_time.duration_since(start_time);

            println!(
                "pattern max {} output max {}, {} msec",
                pattern.iter().max().unwrap(),
                output.iter().max().unwrap(),
                duration.as_secs_f32() * 1e3,
            );
            if vkf != vk::Format::R16G16B16A16_SFLOAT {
                // TODO: Nans need not roundtrip exactly, need a check for this
                assert!(pattern == output);
            }
        }
    }
}
