//! Zero-copy decoding. PyroWave: plane textures are created in wgpu's
//! Vulkan device with exportable memory and imported into a PyroWave
//! device on the same GPU, which decodes straight into them. VAAPI (Linux):
//! decoded surfaces are imported into wgpu as DMA-BUFs.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use ash::vk;
use oriel_media::{ExternalPlane, PYROWAVE_EXTERNAL_MEMORY};
use wgpu::hal::{self, api::Vulkan};

/// One frame's planes: Y, Cb and Cr, or (NV12) Y and interleaved CbCr.
pub struct PlaneSet {
    pub textures: Vec<wgpu::Texture>,
    pub size: (u32, u32),
    pub nv12: bool,
}

pub struct VkInterop {
    device: wgpu::Device,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
    /// DMA-BUFs can be imported (VAAPI zero-copy).
    pub dmabuf: bool,
}

impl VkInterop {
    /// Available when wgpu runs on Vulkan with external memory support.
    pub fn new(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Option<Arc<Self>> {
        if adapter.get_info().backend != wgpu::Backend::Vulkan {
            return None;
        }
        let wanted = if cfg!(windows) {
            wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32
        } else {
            wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD
        };
        if !adapter.features().contains(wanted) {
            tracing::info!("no Vulkan external memory; PyroWave decodes through the CPU");
            return None;
        }
        let hal = unsafe { device.as_hal::<Vulkan>() }?;
        let instance = hal.shared_instance().raw_instance();
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        unsafe { instance.get_physical_device_properties2(hal.raw_physical_device(), &mut props) };
        let (device_uuid, driver_uuid) = (id.device_uuid, id.driver_uuid);
        drop(hal);
        Some(Arc::new(Self {
            device: device.clone(),
            device_uuid,
            driver_uuid,
            dmabuf: device
                .features()
                .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
        }))
    }

    /// A set of 4:2:0 planes for a `w`×`h` frame, plus their exported
    /// memory for PyroWave.
    pub fn create_planes(&self, w: u32, h: u32) -> Result<(PlaneSet, [ExternalPlane; 3])> {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (y, ye) = self.create_plane(w, h)?;
        let (u, ue) = self.create_plane(cw, ch)?;
        let (v, ve) = self.create_plane(cw, ch)?;
        Ok((
            PlaneSet {
                textures: vec![y, u, v],
                size: (w, h),
                nv12: false,
            },
            [ye, ue, ve],
        ))
    }

    fn create_plane(&self, w: u32, h: u32) -> Result<(wgpu::Texture, ExternalPlane)> {
        let hal = unsafe { self.device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
        let raw = hal.raw_device();
        let instance = hal.shared_instance().raw_instance();
        let base = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::STORAGE
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let mut external =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(PYROWAVE_EXTERNAL_MEMORY);
        let info = base.push_next(&mut external);
        unsafe {
            let image = raw.create_image(&info, None)?;
            let req = raw.get_image_memory_requirements(image);
            let props = instance.get_physical_device_memory_properties(hal.raw_physical_device());
            let Some(index) = (0..props.memory_type_count).find(|&i| {
                req.memory_type_bits & (1 << i) != 0
                    && props.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            }) else {
                raw.destroy_image(image, None);
                return Err(anyhow!("no device-local memory for plane textures"));
            };
            let mut export =
                vk::ExportMemoryAllocateInfo::default().handle_types(PYROWAVE_EXTERNAL_MEMORY);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(index)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            let memory = match raw.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    raw.destroy_image(image, None);
                    return Err(e.into());
                }
            };
            raw.bind_image_memory(image, memory, 0)?;
            let handle = export_handle(instance, raw, memory)?;

            let hal_texture = hal.texture_from_raw(
                image,
                &hal::TextureDescriptor {
                    label: Some("pyrowave plane"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUses::RESOURCE,
                    memory_flags: hal::MemoryFlags::empty(),
                    view_formats: vec![],
                },
                None,
                hal::vulkan::TextureMemory::Dedicated(memory),
            );
            drop(hal);
            let texture = self.device.create_texture_from_hal::<Vulkan>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("pyrowave plane"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::RESOURCE,
            );
            Ok((
                texture,
                ExternalPlane {
                    handle,
                    width: w,
                    height: h,
                    info: base,
                },
            ))
        }
    }
}

#[cfg(target_os = "linux")]
impl VkInterop {
    /// Import a VAAPI frame's NV12 planes as textures. The textures share
    /// the surface's memory, so they show whatever it holds next.
    pub fn import_dmabuf(&self, frame: &oriel_media::DmabufFrame) -> Result<PlaneSet> {
        let (w, h) = (frame.width, frame.height);
        let y = self.import_plane(&frame.planes[0], wgpu::TextureFormat::R8Unorm, w, h)?;
        let uv = self.import_plane(
            &frame.planes[1],
            wgpu::TextureFormat::Rg8Unorm,
            w.div_ceil(2),
            h.div_ceil(2),
        )?;
        Ok(PlaneSet {
            textures: vec![y, uv],
            size: (w, h),
            nv12: true,
        })
    }

    fn import_plane(
        &self,
        plane: &oriel_media::DmabufPlane,
        format: wgpu::TextureFormat,
        w: u32,
        h: u32,
    ) -> Result<wgpu::Texture> {
        use std::os::fd::BorrowedFd;
        // The frame keeps its fd; Vulkan takes ownership of a duplicate.
        let fd = unsafe { BorrowedFd::borrow_raw(plane.fd) }.try_clone_to_owned()?;
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let hal = unsafe { self.device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
        let hal_texture = unsafe {
            hal.texture_from_dmabuf_fd(
                fd,
                &hal::TextureDescriptor {
                    label: Some("vaapi plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUses::RESOURCE,
                    memory_flags: hal::MemoryFlags::empty(),
                    view_formats: vec![],
                },
                plane.modifier,
                plane.pitch,
                plane.offset,
            )
        }
        .map_err(|e| anyhow!("import DMA-BUF plane: {e}"))?;
        drop(hal);
        Ok(unsafe {
            self.device.create_texture_from_hal::<Vulkan>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("vaapi plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::RESOURCE,
            )
        })
    }
}

#[cfg(not(windows))]
unsafe fn export_handle(
    instance: &ash::Instance,
    device: &ash::Device,
    memory: vk::DeviceMemory,
) -> Result<usize> {
    let fd_fn = ash::khr::external_memory_fd::Device::new(instance, device);
    let fd = unsafe {
        fd_fn.get_memory_fd(
            &vk::MemoryGetFdInfoKHR::default()
                .memory(memory)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
        )?
    };
    Ok(fd as usize)
}

#[cfg(windows)]
unsafe fn export_handle(
    instance: &ash::Instance,
    device: &ash::Device,
    memory: vk::DeviceMemory,
) -> Result<usize> {
    let win_fn = ash::khr::external_memory_win32::Device::new(instance, device);
    let handle = unsafe {
        win_fn.get_memory_win32_handle(
            &vk::MemoryGetWin32HandleInfoKHR::default()
                .memory(memory)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32),
        )?
    };
    Ok(handle as usize)
}
