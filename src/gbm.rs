/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! DMABUF handling with libgbm; should only be used if Vulkan is not available.
 *
 * To maximize compatibility, newer features and optimizations should be avoided.
 * It is very hard to test them fully without having a variety of old hardware and
 * library versions. To be safe, libgbm functions should only ever be called on
 * the main thread, and mapped memory only accessed from a single thread.
 */
#![cfg(feature = "gbmfallback")]
use crate::tag;
use crate::util::*;
use crate::wayland_gen::WlShmFormat;
use log::{debug, error};
use std::cell::RefCell;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use waypipe_gbm_wrapper::*;

pub struct GBMDevice {
    bindings: gbm,
    device_id: u64,
    device: *mut gbm_device,
    _drm_fd: OwnedFd, /* Keep drm_fd alive, as the gbm device appears to refer to it */
}

/** A type corresponding to a DMABUF object */
pub struct GBMDmabuf {
    /** Reference to keep device alive at least as long as the gbm_bo; the documention does
     * not state that this is necessary, but it also does not state that it isn't. */
    device: Rc<RefCell<GBMDevice>>,
    bo: *mut gbm_bo,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    /* Actual stride of the buffer */
    stride: u32,
    pub modifier: u64,
}

impl Drop for GBMDevice {
    fn drop(&mut self) {
        unsafe {
            (self.bindings.gbm_device_destroy)(self.device);
        }
    }
}
impl Drop for GBMDmabuf {
    fn drop(&mut self) {
        unsafe {
            let dev = self.device.borrow();
            (dev.bindings.gbm_bo_destroy)(self.bo);
        }
    }
}

const LINEAR_MODIFIER: u64 = 0;
const INVALID_MODIFIER: u64 = 0x00ffffffffffffff;

/** Submitting overly large dimensions can make libgbm (or at least, some older version of it)
 * crash, and libgbm does not expose buffer size limits; so set an arbitrary limit which is well under
 * u16::MAX. */
const MAX_DIMENSION: u32 = 16384;

fn get_bpp_if_rgb_planar(fmt: u32) -> Option<u32> {
    use WlShmFormat::*;

    if fmt == fourcc('A', 'R', '2', '4') || fmt == fourcc('X', 'R', '2', '4') {
        return Some(4);
    }

    let f: WlShmFormat = fmt.try_into().ok()?;
    match f {
        Argb8888 | Xrgb8888 => Some(4),
        Rgb332 | Bgr233 => Some(1),
        Xrgb4444 | Xbgr4444 | Rgbx4444 | Bgrx4444 | Argb4444 | Abgr4444 | Rgba4444 | Bgra4444
        | Xrgb1555 | Xbgr1555 | Rgbx5551 | Bgrx5551 | Argb1555 | Abgr1555 | Rgba5551 | Bgra5551
        | Rgb565 | Bgr565 => Some(2),
        Rgb888 | Bgr888 => Some(3),
        Xbgr8888 | Rgbx8888 | Bgrx8888 | Abgr8888 | Rgba8888 | Bgra8888 | Xrgb2101010
        | Xbgr2101010 | Rgbx1010102 | Bgrx1010102 | Argb2101010 | Abgr2101010 | Rgba1010102
        | Bgra1010102 => Some(4),
        R8 => Some(1),
        R16 | Rg88 | Gr88 => Some(2),
        Rg1616 | Gr1616 => Some(4),
        Xrgb16161616f | Xbgr16161616f | Argb16161616f | Abgr16161616f => Some(8),
        Xrgb16161616 | Xbgr16161616 | Argb16161616 | Abgr16161616 => Some(8),
        _ => None,
    }
}

/** Create a GBMDevice, if one with the specified device id exists */
pub fn setup_gbm_device(device: Option<u64>) -> Result<Option<Rc<RefCell<GBMDevice>>>, String> {
    let id_list = if let Some(d) = device {
        vec![d]
    } else {
        list_render_device_ids()
    };
    if id_list.is_empty() {
        return Ok(None);
    }
    unsafe {
        let bindings = match gbm::new("libgbm.so") {
            Err(x) => {
                error!("Failed to load libgbm.so: {}", x);
                return Ok(None);
            }
            Ok(x) => x,
        };

        for id in id_list {
            debug!("Trying to set up gbm device at id: {:x}", id);
            let render_fd = match drm_open_render((id & 0xff) as u32, true) {
                Ok(x) => x,
                Err(_) => continue,
            };

            let dev = bindings.gbm_create_device(render_fd.as_raw_fd());
            if dev.is_null() {
                continue;
            }

            return Ok(Some(Rc::new(RefCell::new(GBMDevice {
                bindings,
                device: dev,
                device_id: id,
                _drm_fd: render_fd,
            }))));
        }
    }
    Ok(None)
}

/** Import a dmabuf. */
pub fn gbm_import_dmabuf(
    device: &Rc<RefCell<GBMDevice>>,
    mut planes: Vec<AddDmabufPlane>,
    width: u32,
    height: u32,
    drm_format: u32,
) -> Result<GBMDmabuf, String> {
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(tag!(
            "DMABUF size to import is too large: ({},{}) > ({},{})",
            width,
            height,
            MAX_DIMENSION,
            MAX_DIMENSION
        ));
    }
    if planes.len() != 1 {
        return Err(tag!(
            "Received {} DMABUF planes when single plane expected",
            planes.len(),
        ));
    };
    let plane = planes.pop().unwrap();
    if plane.plane_idx != 0 {
        return Err(tag!("Incorrect plane index {}!=0", plane.plane_idx,));
    }
    if plane.offset != 0 {
        return Err(tag!(
            "Expected zero offset for gbm import, not {}",
            plane.offset,
        ));
    }
    let flags = match plane.modifier {
        LINEAR_MODIFIER => gbm_bo_flags_GBM_BO_USE_LINEAR | gbm_bo_flags_GBM_BO_USE_RENDERING,
        INVALID_MODIFIER => gbm_bo_flags_GBM_BO_USE_RENDERING,
        _ => {
            return Err(tag!(
                "Importing is only supported with invalid/unspecified or linear modifier, not {:#016x}", plane.modifier,
            ));
        }
    };
    let modifier = plane.modifier;
    let stride = plane.stride;

    let dev = device.borrow_mut();
    let mut data = gbm_import_fd_data {
        fd: plane.fd.as_raw_fd(),
        width,
        height,
        stride,
        format: drm_format,
    };
    unsafe {
        let bo = dev.bindings.gbm_bo_import(
            dev.device,
            GBM_BO_IMPORT_FD,
            &mut data as *mut gbm_import_fd_data as *mut c_void,
            flags,
        );
        /* Keep the fd alive until after the import. */
        drop(plane);
        if bo.is_null() {
            return Err(tag!(
                "Failed to import DMABUF with (format, modifier) = ({:#08x}, {:#016x})",
                drm_format,
                modifier,
            ));
        }

        Ok(GBMDmabuf {
            device: device.clone(),
            bo,
            width,
            height,
            stride,
            format: drm_format,
            modifier,
        })
    }
}

/** Create a dmabuf with the specified properties and a modifier chosen from the list, if possible. */
pub fn gbm_create_dmabuf(
    device: &Rc<RefCell<GBMDevice>>,
    width: u32,
    height: u32,
    format: u32,
    modifier_options: &[u64],
) -> Result<(GBMDmabuf, Vec<AddDmabufPlane>), String> {
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(tag!(
            "DMABUF size to create is too large: ({},{}) > ({},{})",
            width,
            height,
            MAX_DIMENSION,
            MAX_DIMENSION
        ));
    }
    let (flags, actual_mod) = if modifier_options.contains(&LINEAR_MODIFIER) {
        (
            gbm_bo_flags_GBM_BO_USE_RENDERING | gbm_bo_flags_GBM_BO_USE_LINEAR,
            LINEAR_MODIFIER,
        )
    } else if modifier_options.contains(&INVALID_MODIFIER) {
        (gbm_bo_flags_GBM_BO_USE_RENDERING, INVALID_MODIFIER)
    } else {
        return Err(tag!(
            "Unsupported DMABUF modifier options: ({:#08x},{:#016x?})",
            format,
            modifier_options,
        ));
    };

    if get_bpp_if_rgb_planar(format).is_none() {
        return Err(tag!(
            "Unsupported DMABUF format or modifier: ({:#08x},{:#016x?})",
            format,
            modifier_options,
        ));
    }

    let dev = device.borrow_mut();
    unsafe {
        let bo = (dev.bindings.gbm_bo_create)(dev.device, width, height, format, flags);
        if bo.is_null() {
            return Err(tag!(
                "Failed to create DMABUF with (format, modifier) = ({:#08x}, {:#016x})",
                format,
                actual_mod,
            ));
        }
        let fd = match (dev.bindings.gbm_bo_get_fd)(bo) {
            -1 => {
                (dev.bindings.gbm_bo_destroy)(bo);
                return Err(tag!(
                    "Failed to export DMABUF with (format, modifier) = ({:#08x}, {:#016x})",
                    format,
                    actual_mod,
                ));
            }
            x => OwnedFd::from_raw_fd(x),
        };

        /* No failure mechanism is documented */
        let stride = (dev.bindings.gbm_bo_get_stride)(bo);
        Ok((
            GBMDmabuf {
                device: device.clone(),
                bo,
                width,
                height,
                format,
                stride,
                modifier: actual_mod,
            },
            vec![AddDmabufPlane {
                fd,
                plane_idx: 0,
                /* gbm_bo_get_offset was added in 2016 and appears to be used only for plane indices;
                 */
                offset: 0,
                stride,
                modifier: actual_mod,
            }],
        ))
    }
}

enum MapType {
    Read,
    WriteAll,
}

/** Map a dmabuf using gbm's API.
 *
 * It is unclear how safe multi-threaded access to buffers.
 */
unsafe fn map_dmabuf(
    bindings: &gbm,
    bo: *mut gbm_bo,
    width: u32,
    height: u32,
    map: MapType,
) -> Result<(*mut u8, u32, *mut c_void), String> {
    /* With i965, the map handle MUST initially point to a NULL pointer; otherwise
     * the handler may silently exit, sometimes with misleading errno :-( */
    let mut map_handle: *mut c_void = std::ptr::null_mut();
    /* As of 2022, with amdgpu, GBM_BO_TRANSFER_WRITE invalidates
     * regions not written to during the mapping, while iris preserves
     * the original buffer contents. GBM documentation does not say which
     * WRITE behavior is correct. What the individual drivers do may change
     * in the future. Specifying READ_WRITE preserves the old contents with
     * both drivers. */
    let flags = match map {
        MapType::Read => gbm_bo_transfer_flags_GBM_BO_TRANSFER_READ,
        MapType::WriteAll => gbm_bo_transfer_flags_GBM_BO_TRANSFER_WRITE,
    };
    let mut stride = 0;
    let data = (bindings.gbm_bo_map)(bo, 0, 0, width, height, flags, &mut stride, &mut map_handle);
    if data.is_null() {
        return Err(tag!("Failed to map dmabuf with gbm"));
    }
    Ok((data as *mut u8, stride, map_handle))
}
unsafe fn unmap_dmabuf(bindings: &gbm, bo: *mut gbm_bo, handle: *mut c_void) {
    (bindings.gbm_bo_unmap)(bo, handle);
}

fn stride_adjusted_copy(dst: &mut [u8], dst_stride: u32, src: &[u8], src_stride: u32, height: u32) {
    let common = dst_stride.min(src_stride);
    for row in 0..height {
        dst[(dst_stride * row) as usize..((dst_stride * row) + common) as usize].copy_from_slice(
            &src[(src_stride * row) as usize..((src_stride * row) + common) as usize],
        )
    }
}

impl GBMDmabuf {
    /** Copy out the entire contents of the dmabuf onto an array (which is either densely
     * packed or uses the nominal stride. */
    pub fn copy_from_dmabuf(
        &mut self,
        view_row_stride: Option<u32>,
        data: &mut [u8],
    ) -> Result<(), String> {
        let dev = self.device.borrow_mut();
        let data_stride = view_row_stride.unwrap_or(
            self.width
                .checked_mul(get_bpp_if_rgb_planar(self.format).unwrap())
                .unwrap(),
        );

        unsafe {
            let (map_data, map_stride, map_handle) = map_dmabuf(
                &dev.bindings,
                self.bo,
                self.width,
                self.height,
                MapType::Read,
            )?;

            let mapped_length: usize = map_stride
                .checked_mul(self.height)
                .unwrap()
                .try_into()
                .unwrap();
            assert!(mapped_length <= isize::MAX as usize);

            let mapped_region = std::slice::from_raw_parts(map_data, mapped_length);
            stride_adjusted_copy(data, data_stride, mapped_region, map_stride, self.height);

            unmap_dmabuf(&dev.bindings, self.bo, map_handle);
        }

        Ok(())
    }
    /** Copy data onto the dmabuf. */
    pub fn copy_onto_dmabuf(
        &mut self,
        view_row_stride: Option<u32>,
        data: &[u8],
    ) -> Result<(), String> {
        let dev = self.device.borrow_mut();
        let data_stride = view_row_stride.unwrap_or(
            self.width
                .checked_mul(get_bpp_if_rgb_planar(self.format).unwrap())
                .unwrap(),
        );

        unsafe {
            let (map_data, map_stride, map_handle) = map_dmabuf(
                &dev.bindings,
                self.bo,
                self.width,
                self.height,
                MapType::WriteAll,
            )?;

            let mapped_length: usize = map_stride
                .checked_mul(self.height)
                .unwrap()
                .try_into()
                .unwrap();
            assert!(mapped_length <= isize::MAX as usize);

            let mapped_region = std::slice::from_raw_parts_mut(map_data, mapped_length);
            stride_adjusted_copy(mapped_region, map_stride, data, data_stride, self.height);

            unmap_dmabuf(&dev.bindings, self.bo, map_handle);
        }

        Ok(())
    }

    // TODO: deduplicate with Vulkan
    pub fn nominal_size(&self, view_row_length: Option<u32>) -> usize {
        if let Some(r) = view_row_length {
            (self.height * r) as usize
        } else {
            let bpp = get_bpp_if_rgb_planar(self.format).unwrap();
            (self.width * self.height * bpp) as usize
        }
    }

    pub fn ideal_slice_data(&self) -> [u8; 64] {
        // TODO: deduplicate with Vulkan
        let mut out = [0; 64];
        out[0..4].copy_from_slice(&self.width.to_le_bytes());
        out[4..8].copy_from_slice(&self.height.to_le_bytes());
        out[8..12].copy_from_slice(&self.format.to_le_bytes());
        out[12..16].copy_from_slice(&1u32.to_le_bytes());

        let offset = 0_u32;
        out[16..20].copy_from_slice(&offset.to_le_bytes());
        out[32..36].copy_from_slice(&self.stride.to_le_bytes());

        /* This modifier is only ever used by waypipe-c */
        out[48..56].copy_from_slice(&self.modifier.to_le_bytes());
        /* Link plane to dmabuf */
        out[56] = 1;

        out
    }
}

/** Return supported GBM modifiers for a format, or empty list if format not supported.
 *
 * Restrict to known single-plane RGB-type formats, and to LINEAR or INVALID modifiers.
 * Other modifiers are not supported, because a) they may require auxiliary control planes
 * or other features which are awkward or impossible to use in all versions of libgbm; b)
 * performance can be terrible (using e.g. Strong Uncacheable mappings that forbid pipelining
 * or caching read/write operations). */
pub fn gbm_supported_modifiers(device: &Rc<RefCell<GBMDevice>>, format: u32) -> Vec<u64> {
    let dev = device.borrow_mut();

    // TODO: on startup, iterate over all candidate planar formats and evaluate these once.
    let mut mods = Vec::new();
    if get_bpp_if_rgb_planar(format).is_none() {
        return mods;
    }
    unsafe {
        if (dev.bindings.gbm_device_is_format_supported)(
            dev.device,
            format,
            gbm_bo_flags_GBM_BO_USE_RENDERING,
        ) == 1
        {
            mods.push(INVALID_MODIFIER);
        }
        if (dev.bindings.gbm_device_is_format_supported)(
            dev.device,
            format,
            gbm_bo_flags_GBM_BO_USE_RENDERING | gbm_bo_flags_GBM_BO_USE_LINEAR,
        ) == 1
        {
            mods.push(LINEAR_MODIFIER);
        }
    }
    mods
}
/** Get the dev_t identifying the device. */
pub fn gbm_get_device_id(device: &Rc<RefCell<GBMDevice>>) -> u64 {
    let dev = device.borrow_mut();
    dev.device_id
}
