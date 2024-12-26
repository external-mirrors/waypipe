/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! No-op implementations to build with specific features disabled */
#![allow(unused_variables)]
#[cfg(not(feature = "dmabuf"))]
mod dmabuf_stub {
    use std::os::fd::{BorrowedFd, OwnedFd};
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::VideoSetting;
    pub struct AddDmabufPlane {
        pub fd: OwnedFd,
        pub plane_idx: u32,
        pub offset: u32,
        pub stride: u32,
        pub modifier: u64,
    }
    pub struct VulkanInstance(());
    pub struct VulkanDevice(());
    pub struct VulkanCommandPool {
        pub vulk: Arc<VulkanDevice>,
    }
    pub struct VulkanTimelineSemaphore(());
    pub struct VulkanCopyHandle(());
    pub struct VulkanDmabuf {
        pub vulk: Arc<VulkanDevice>,
        pub width: usize,
        pub height: usize,
    }
    pub struct VulkanBuffer(());
    pub struct VulkanBufferReadView<'a> {
        pub data: &'a [u8],
    }
    pub struct VulkanBufferWriteView<'a> {
        pub data: &'a mut [u8],
    }

    pub fn setup_vulkan_instance(
        debug: bool,
        video: &VideoSetting,
    ) -> Result<Arc<VulkanInstance>, String> {
        unreachable!();
    }
    pub fn setup_vulkan_device_base(
        instance: &Arc<VulkanInstance>,
        main_device: Option<u64>,
        format_filter_for_video: bool,
    ) -> Result<VulkanDevice, String> {
        unreachable!();
    }
    pub fn setup_vulkan_device(
        instance: &Arc<VulkanInstance>,
        main_device: Option<u64>,
        video: &VideoSetting,
        debug: bool,
    ) -> Result<Arc<VulkanDevice>, String> {
        unreachable!();
    }

    pub fn start_copy_segments_from_dmabuf(
        img: &Arc<VulkanDmabuf>,
        copy: &Arc<VulkanBuffer>,
        pool: &Arc<VulkanCommandPool>,
        segments: &[(u32, u32, u32)],
        view_row_length: Option<u32>,
        wait_semaphores: &[(Arc<VulkanTimelineSemaphore>, u64)],
    ) -> Result<VulkanCopyHandle, String> {
        unreachable!();
    }
    pub fn start_copy_segments_onto_dmabuf(
        img: &Arc<VulkanDmabuf>,
        copy: &Arc<VulkanBuffer>,
        pool: &Arc<VulkanCommandPool>,
        segments: &[(u32, u32, u32)],
        view_row_length: Option<u32>,
        wait_semaphores: &[(Arc<VulkanTimelineSemaphore>, u64)],
    ) -> Result<VulkanCopyHandle, String> {
        unreachable!();
    }
    pub fn vulkan_get_cmd_pool(
        vulk: &Arc<VulkanDevice>,
    ) -> Result<Arc<VulkanCommandPool>, &'static str> {
        unreachable!();
    }
    pub fn vulkan_import_timeline(
        vulk: &Arc<VulkanDevice>,
        fd: OwnedFd,
    ) -> Result<Arc<VulkanTimelineSemaphore>, String> {
        unreachable!();
    }
    pub fn vulkan_create_timeline(
        vulk: &Arc<VulkanDevice>,
        start_pt: u64,
    ) -> Result<(Arc<VulkanTimelineSemaphore>, OwnedFd), String> {
        unreachable!();
    }
    pub fn drm_to_wayland(drm_format: u32) -> u32 {
        unreachable!();
    }
    pub fn get_dev_for_drm_node_path(path: &PathBuf) -> Result<u64, &'static str> {
        unreachable!();
    }
    pub fn vulkan_get_buffer(
        vulk: &Arc<VulkanDevice>,
        nom_len: usize,
        read_optimized: bool,
    ) -> Result<VulkanBuffer, &'static str> {
        unreachable!();
    }
    pub fn vulkan_create_dmabuf(
        vulk: &Arc<VulkanDevice>,
        width: u32,
        height: u32,
        drm_format: u32,
        modifier_options: &[u64],
        can_store_and_sample: bool,
    ) -> Result<(Arc<VulkanDmabuf>, Vec<AddDmabufPlane>), String> {
        unreachable!();
    }
    pub fn vulkan_import_dmabuf(
        vulk: &Arc<VulkanDevice>,
        planes: Vec<AddDmabufPlane>,
        width: u32,
        height: u32,
        drm_format: u32,
        can_store_and_sample: bool,
    ) -> Result<Arc<VulkanDmabuf>, String> {
        unreachable!();
    }
    impl VulkanBuffer {
        pub fn prepare_read(self: &VulkanBuffer) -> Result<(), &'static str> {
            unreachable!();
        }
        pub fn complete_write(self: &VulkanBuffer) -> Result<(), &'static str> {
            unreachable!();
        }
        pub fn get_read_view(self: &VulkanBuffer) -> VulkanBufferReadView {
            unreachable!();
        }
        pub fn get_write_view(self: &VulkanBuffer) -> VulkanBufferWriteView {
            unreachable!();
        }
    }
    impl VulkanCopyHandle {
        pub fn get_timeline_point(self: &VulkanCopyHandle) -> u64 {
            unreachable!();
        }
    }
    impl VulkanInstance {
        pub fn has_device(&self, main_device: Option<u64>) -> bool {
            unreachable!();
        }
    }
    impl VulkanDevice {
        pub fn wait_for_timeline_pt(&self, pt: u64, max_wait: u64) -> Result<bool, &'static str> {
            unreachable!();
        }
        pub fn get_device(&self) -> u64 {
            unreachable!();
        }
        pub fn get_event_fd(&self, timeline_point: u64) -> Result<BorrowedFd, String> {
            unreachable!();
        }
        pub fn get_current_timeline_pt(&self) -> Result<u64, &'static str> {
            unreachable!();
        }
        pub fn supports_format(&self, drm_format: u32, drm_modifier: u64) -> bool {
            unreachable!();
        }
        pub fn get_supported_modifiers(&self, drm_format: u32) -> Vec<u64> {
            unreachable!();
        }
        pub fn can_import_image(
            &self,
            drm_format: u32,
            width: u32,
            height: u32,
            planes: &[AddDmabufPlane],
            can_store_and_sample: bool,
        ) -> bool {
            unreachable!();
        }
    }

    impl VulkanDmabuf {
        pub fn nominal_size(self: &VulkanDmabuf, view_row_length: Option<u32>) -> usize {
            unreachable!();
        }
        pub fn get_bpp(&self) -> usize {
            unreachable!();
        }
        pub fn ideal_slice_data(self: &VulkanDmabuf, drm_format: u32) -> [u8; 64] {
            unreachable!();
        }
        pub fn get_first_stride(data: [u8; 64]) -> u32 {
            unreachable!();
        }
    }

    impl VulkanTimelineSemaphore {
        pub fn get_current_pt(self: &VulkanTimelineSemaphore) -> Result<u64, &'static str> {
            unreachable!();
        }

        pub fn get_event_fd(self: &VulkanTimelineSemaphore) -> BorrowedFd {
            unreachable!();
        }
        pub fn link_event_fd(
            self: &VulkanTimelineSemaphore,
            timeline_point: u64,
        ) -> Result<BorrowedFd, &'static str> {
            unreachable!();
        }
        pub fn signal_timeline_pt(self: &VulkanTimelineSemaphore, pt: u64) -> Result<(), String> {
            unreachable!();
        }
    }
}
#[cfg(not(feature = "dmabuf"))]
pub use dmabuf_stub::*;

#[cfg(not(feature = "video"))]
mod video_stub {
    use std::sync::Arc;

    #[cfg(feature = "dmabuf")]
    use crate::dmabuf::*;
    #[cfg(not(feature = "dmabuf"))]
    use crate::stub::*;
    use crate::VideoFormat;

    pub struct VideoEncodeState(());
    pub struct VideoDecodeState(());
    pub struct VulkanDecodeOpHandle(());

    pub fn start_dmavid_apply(
        state: &Arc<VideoDecodeState>,
        pool: &Arc<VulkanCommandPool>,
        packet: &[u8],
    ) -> Result<VulkanDecodeOpHandle, String> {
        unreachable!();
    }
    pub fn start_dmavid_encode(
        state: &Arc<VideoEncodeState>,
        pool: &Arc<VulkanCommandPool>,
    ) -> Result<Vec<u8>, String> {
        unreachable!();
    }
    pub fn setup_video_decode(
        img: &Arc<VulkanDmabuf>,
        fmt: VideoFormat,
    ) -> Result<VideoDecodeState, &'static str> {
        unreachable!();
    }
    pub fn setup_video_encode(
        img: &Arc<VulkanDmabuf>,
        fmt: VideoFormat,
        bpf: Option<f32>,
    ) -> Result<VideoEncodeState, &'static str> {
        unreachable!();
    }
    pub fn supports_video_format(
        vulk: &VulkanDevice,
        fmt: VideoFormat,
        drm_format: u32,
        width: u32,
        height: u32,
    ) -> bool {
        false
    }
    impl VulkanDecodeOpHandle {
        pub fn get_timeline_point(&self) -> u64 {
            unreachable!();
        }
    }
}
#[cfg(not(feature = "video"))]
pub use video_stub::*;
