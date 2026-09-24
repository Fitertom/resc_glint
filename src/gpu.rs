//! Vulkan: one device, one swapchain, one pipeline, a few textures. Vulkan 1.3 core —
//! dynamic rendering and synchronization2 — so there are no render passes or framebuffers to
//! rebuild on every resize.
//!
//! **One frame in flight.** A viewer draws only when something changes; the second frame in
//! flight would buy throughput nobody needs and make every texture swap wait on two fences.

use ash::khr::{surface, swapchain, win32_surface};
use ash::vk;

/// Photos kept on the GPU at once: the current one and its neighbours, uploaded ahead so a
/// flip is a draw and nothing else.
pub const IMAGE_SLOTS: usize = 4;
pub const SLOT_CAPTION: usize = IMAGE_SLOTS;
pub const SLOT_INFO: usize = IMAGE_SLOTS + 1;
pub const SLOT_CARD: usize = IMAGE_SLOTS + 2;
pub const SLOT_BAR: usize = IMAGE_SLOTS + 3;
pub const SLOT_TIP: usize = IMAGE_SLOTS + 4;
const SLOTS: usize = IMAGE_SLOTS + 5;

/// The staging buffer kept while hidden, and the least one made: a thumbnail, a caption and a
/// screen-sized preview fit.
const KEPT_STAGING: usize = 16 << 20;
/// Textures up to this much memory (a thumbnail, the caption, the toolbar) get a block this
/// size, handed back to a pool rather than freed: allocating GPU memory costs ~1 ms, and a
/// hidden viewer shown again would pay it for every one on the way to its first frame.
const SMALL_BLOCK: u64 = 1 << 20;
const SMALL_KEPT: usize = 8;
/// Staging kept in the working set while hidden: what the first frame of an open writes.
const WARM_STAGING: usize = 1 << 20;

/// Trace the parts of each frame (while an open is being measured).
pub static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What one quad needs, laid out as the shaders' push constants.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Quad {
    /// x0, y0, x1, y1 in normalised device coordinates.
    pub rect: [f32; 4],
    /// Corner → uv matrix, column-major 2x2.
    pub m: [f32; 4],
    /// uv offset, screen pixels per texel, overlay flag.
    pub o: [f32; 4],
    /// Background rgb and the checkerboard flag.
    pub bg: [f32; 4],
}

struct Texture {
    image: vk::Image,
    mem: vk::DeviceMemory,
    /// The memory is a pooled `SMALL_BLOCK` of this type.
    pooled: Option<u32>,
    view: vk::ImageView,
    w: u32,
    h: u32,
    mips: u32,
}

struct Staging {
    buf: vk::Buffer,
    mem: vk::DeviceMemory,
    ptr: *mut u8,
    size: usize,
    used: usize,
}

struct Upload {
    slot: usize,
    offset: usize,
}

pub struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    surface_fn: surface::Instance,
    surface: vk::SurfaceKHR,
    pdev: vk::PhysicalDevice,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    pub max_dim: u32,
    device: ash::Device,
    queue: vk::Queue,
    sw_fn: swapchain::Device,
    swapchain: vk::SwapchainKHR,
    format: vk::Format,
    pub extent: vk::Extent2D,
    present_mode: vk::PresentModeKHR,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    /// ONE PER SWAPCHAIN IMAGE, not per frame (as in `resc_render`): a frame's semaphore could
    /// be reused while the present of image K still waits on it.
    present_sems: Vec<vk::Semaphore>,
    acquire_sem: vk::Semaphore,
    fence: vk::Fence,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    set_layout: vk::DescriptorSetLayout,
    pipe_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    desc_pool: vk::DescriptorPool,
    sets: [vk::DescriptorSet; SLOTS],
    tex: [Option<Texture>; SLOTS],
    staging: Option<Staging>,
    uploads: Vec<Upload>,
    /// Free `SMALL_BLOCK`s with their memory type.
    small: std::cell::RefCell<Vec<(vk::DeviceMemory, u32)>>,
    want_rebuild: bool,
    wanted: (u32, u32),
}

fn vkerr(what: &str) -> impl Fn(vk::Result) -> String + '_ {
    move |e| format!("{what}: {e:?}")
}

impl Gpu {
    pub fn new(hwnd: isize, size: (u32, u32), prefer_integrated: bool) -> Result<Gpu, String> {
        // SAFETY: the Vulkan calls below follow the spec's order; every handle made here is
        // destroyed in `Drop`, after the device is idle.
        unsafe {
            let entry = ash::Entry::load().map_err(|e| format!("Vulkan is not installed: {e}"))?;
            crate::trace("vk: loader");
            let version = entry.try_enumerate_instance_version().ok().flatten().unwrap_or(vk::API_VERSION_1_0);
            if version < vk::API_VERSION_1_3 {
                return Err("Vulkan 1.3 is required (update the GPU driver)".into());
            }
            let app = vk::ApplicationInfo::default().application_name(c"Glint").api_version(vk::API_VERSION_1_3);
            let exts = [surface::NAME.as_ptr(), win32_surface::NAME.as_ptr()];
            let instance = entry
                .create_instance(&vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&exts), None)
                .map_err(vkerr("vkCreateInstance"))?;
            crate::trace("vk: instance");
            let surface_fn = surface::Instance::new(&entry, &instance);
            let hinst = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            let surface = win32_surface::Instance::new(&entry, &instance)
                .create_win32_surface(&vk::Win32SurfaceCreateInfoKHR::default().hinstance(hinst as isize).hwnd(hwnd), None)
                .map_err(vkerr("vkCreateWin32SurfaceKHR"))?;

            // A device that is 1.3 and can show in this window; the preferred kind first.
            let mut best: Option<(vk::PhysicalDevice, u32, i32)> = None;
            for pd in instance.enumerate_physical_devices().map_err(vkerr("devices"))? {
                let props = instance.get_physical_device_properties(pd);
                if props.api_version < vk::API_VERSION_1_3 {
                    continue;
                }
                let fams = instance.get_physical_device_queue_family_properties(pd);
                let Some(q) = (0..fams.len() as u32).find(|&i| {
                    fams[i as usize].queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        && surface_fn.get_physical_device_surface_support(pd, i, surface).unwrap_or(false)
                }) else {
                    continue;
                };
                let score = match props.device_type {
                    vk::PhysicalDeviceType::DISCRETE_GPU => if prefer_integrated { 2 } else { 3 },
                    vk::PhysicalDeviceType::INTEGRATED_GPU => if prefer_integrated { 3 } else { 2 },
                    _ => 1,
                };
                if best.is_none_or(|b| score > b.2) {
                    best = Some((pd, q, score));
                }
            }
            let (pdev, qfam, _) = best.ok_or("no Vulkan 1.3 GPU can show in this window")?;
            let props = instance.get_physical_device_properties(pdev);
            let mem_props = instance.get_physical_device_memory_properties(pdev);

            let prio = [1.0];
            let qinfo = [vk::DeviceQueueCreateInfo::default().queue_family_index(qfam).queue_priorities(&prio)];
            let dexts = [swapchain::NAME.as_ptr()];
            let mut f13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true).synchronization2(true);
            let device = instance
                .create_device(pdev, &vk::DeviceCreateInfo::default().queue_create_infos(&qinfo).enabled_extension_names(&dexts).push_next(&mut f13), None)
                .map_err(vkerr("vkCreateDevice"))?;
            crate::trace("vk: device");
            let queue = device.get_device_queue(qfam, 0);
            let sw_fn = swapchain::Device::new(&instance, &device);

            // PLAIN UNORM, no sRGB: the file's bytes go to the screen as they are, and the
            // texture is filtered in the same space, as every image viewer does.
            let formats = surface_fn.get_physical_device_surface_formats(pdev, surface).map_err(vkerr("formats"))?;
            let format = formats
                .iter()
                .find(|f| f.format == vk::Format::B8G8R8A8_UNORM && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
                .map_or(formats[0].format, |f| f.format);
            // MAILBOX where there is one: no tearing, and a pan follows the hand without a
            // queue of frames behind it. Frames are drawn only on change, so it never spins.
            let modes = surface_fn.get_physical_device_surface_present_modes(pdev, surface).unwrap_or_default();
            let present_mode = if modes.contains(&vk::PresentModeKHR::MAILBOX) { vk::PresentModeKHR::MAILBOX } else { vk::PresentModeKHR::FIFO };

            let sem = || device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None);
            let acquire_sem = sem().map_err(vkerr("semaphore"))?;
            let fence = device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None).map_err(vkerr("fence"))?;
            let pool = device
                .create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(qfam).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None)
                .map_err(vkerr("pool"))?;
            let cmd = device
                .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))
                .map_err(vkerr("cmd"))?[0];

            let binding = [vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
            let set_layout = device.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding), None).map_err(vkerr("set layout"))?;
            let range = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                .size(size_of::<Quad>() as u32)];
            let layouts = [set_layout];
            let pipe_layout = device
                .create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts).push_constant_ranges(&range), None)
                .map_err(vkerr("pipeline layout"))?;
            let pipeline = make_pipeline(&device, pipe_layout, format)?;
            let sampler = device
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .max_lod(vk::LOD_CLAMP_NONE),
                    None,
                )
                .map_err(vkerr("sampler"))?;
            let sizes = [vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: SLOTS as u32 }];
            let desc_pool = device
                .create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(SLOTS as u32).pool_sizes(&sizes), None)
                .map_err(vkerr("descriptor pool"))?;
            let all = [set_layout; SLOTS];
            let v = device
                .allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(desc_pool).set_layouts(&all))
                .map_err(vkerr("descriptor sets"))?;

            let mut g = Gpu {
                _entry: entry,
                instance,
                surface_fn,
                surface,
                pdev,
                mem_props,
                max_dim: props.limits.max_image_dimension2_d,
                device,
                queue,
                sw_fn,
                swapchain: vk::SwapchainKHR::null(),
                format,
                extent: vk::Extent2D::default(),
                present_mode,
                images: Vec::new(),
                views: Vec::new(),
                present_sems: Vec::new(),
                acquire_sem,
                fence,
                pool,
                cmd,
                set_layout,
                pipe_layout,
                pipeline,
                sampler,
                desc_pool,
                sets: std::array::from_fn(|i| v[i]),
                tex: Default::default(),
                staging: None,
                uploads: Vec::new(),
                small: Default::default(),
                want_rebuild: false,
                wanted: size,
            };
            crate::trace("vk: pipeline");
            g.rebuild()?;
            crate::trace("vk: swapchain");
            Ok(g)
        }
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.wanted = (w, h);
        self.want_rebuild = true;
    }

    pub fn has(&self, slot: usize) -> bool {
        self.tex[slot].is_some()
    }

    pub fn size_of(&self, slot: usize) -> (u32, u32) {
        self.tex[slot].as_ref().map_or((0, 0), |t| (t.w, t.h))
    }

    /// Free the staging buffer: a big photo grew it to 100+ MB, and a hidden viewer should not
    /// keep that.
    pub fn release_staging(&mut self) {
        self.wait();
        self.uploads.clear();
        // A SMALL ONE STAYS: made anew on the next open it cost ~20 ms of allocation and first
        // touches, right on the way to the first frame.
        if self.staging.as_ref().is_some_and(|s| s.size <= KEPT_STAGING) {
            return;
        }
        if let Some(s) = self.staging.take() {
            // SAFETY: the fence is waited; nothing reads the buffer.
            unsafe {
                self.device.destroy_buffer(s.buf, None);
                self.device.free_memory(s.mem, None);
            }
        }
    }

    pub fn clear(&mut self, slot: usize) {
        self.wait();
        self.uploads.retain(|u| u.slot != slot);
        if let Some(t) = self.tex[slot].take() {
            self.destroy_texture(t);
        }
    }

    /// Premultiplied BGRA `w × h` into a slot, sent with the next frame.
    pub fn upload(&mut self, slot: usize, w: u32, h: u32, px: &[u8]) -> Result<(), String> {
        self.upload_image(slot, w, h, px, false)
    }

    /// The same, with a mip chain when `mipmapped`: without one a 6000-pixel photo shown at
    /// 1000 shimmers and aliases. A thumbnail goes without: it is only ever shown enlarged,
    /// and building the chain was most of the first frame's GPU work.
    pub fn upload_image(&mut self, slot: usize, w: u32, h: u32, px: &[u8], mipmapped: bool) -> Result<(), String> {
        let mips = if mipmapped { 32 - w.max(h).leading_zeros() } else { 1 };
        self.wait();
        self.uploads.retain(|u| u.slot != slot);
        let reuse = self.tex[slot].as_ref().is_some_and(|t| t.w == w && t.h == h && t.mips == mips);
        if !reuse {
            if let Some(t) = self.tex[slot].take() {
                self.destroy_texture(t);
            }
            let t = self.make_texture(w, h, mips)?;
            // SAFETY: the set is not in use (fence waited above); the view is live.
            unsafe {
                let info = [vk::DescriptorImageInfo { sampler: self.sampler, image_view: t.view, image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL }];
                let write = vk::WriteDescriptorSet::default()
                    .dst_set(self.sets[slot])
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&info);
                self.device.update_descriptor_sets(&[write], &[]);
            }
            self.tex[slot] = Some(t);
        }
        let offset = self.stage(px)?;
        self.uploads.push(Upload { slot, offset });
        Ok(())
    }

    /// Draw the quads, each with its slot's texture, and show the frame. False when there is
    /// nothing to draw into (minimised) — the caller asks again on the next change.
    pub fn draw(&mut self, clear: [f32; 3], quads: &[(usize, Quad)]) -> Result<bool, String> {
        // An out-of-date swapchain is rebuilt and the frame tried once more: nothing else
        // would ask for it until the next event.
        match self.draw_once(clear, quads)? {
            true => Ok(true),
            false => self.draw_once(clear, quads),
        }
    }

    fn draw_once(&mut self, clear: [f32; 3], quads: &[(usize, Quad)]) -> Result<bool, String> {
        // SAFETY: one frame in flight: after the fence, nothing recorded before is running.
        unsafe {
            if self.want_rebuild {
                self.rebuild()?;
            }
            if self.extent.width == 0 || self.extent.height == 0 {
                return Ok(false);
            }
            let t0 = std::time::Instant::now();
            self.wait();
            let t_fence = t0.elapsed();
            let idx = match self.sw_fn.acquire_next_image(self.swapchain, u64::MAX, self.acquire_sem, vk::Fence::null()) {
                Ok((i, suboptimal)) => {
                    self.want_rebuild |= suboptimal;
                    i as usize
                }
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.rebuild()?;
                    return Ok(false);
                }
                Err(e) => return Err(format!("acquire: {e:?}")),
            };
            let t_acq = t0.elapsed();
            let d = &self.device;
            let cmd = self.cmd;
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).map_err(vkerr("reset"))?;
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
                .map_err(vkerr("begin"))?;
            self.record_uploads();

            let d = &self.device;
            let target = self.images[idx];
            barrier(d, cmd, target, 0, 1, vk::ImageLayout::UNDEFINED, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                (vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::NONE),
                (vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE));
            let att = [vk::RenderingAttachmentInfo::default()
                .image_view(self.views[idx])
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue { color: vk::ClearColorValue { float32: [clear[0], clear[1], clear[2], 1.0] } })];
            let area = vk::Rect2D { offset: vk::Offset2D::default(), extent: self.extent };
            d.cmd_begin_rendering(cmd, &vk::RenderingInfo::default().render_area(area).layer_count(1).color_attachments(&att));
            let vp = vk::Viewport { x: 0.0, y: 0.0, width: self.extent.width as f32, height: self.extent.height as f32, min_depth: 0.0, max_depth: 1.0 };
            d.cmd_set_viewport(cmd, 0, &[vp]);
            d.cmd_set_scissor(cmd, 0, &[area]);
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            for (slot, q) in quads {
                if self.tex[*slot].is_none() {
                    continue;
                }
                d.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipe_layout, 0, &[self.sets[*slot]], &[]);
                let bytes = std::slice::from_raw_parts((q as *const Quad).cast::<u8>(), size_of::<Quad>());
                d.cmd_push_constants(cmd, self.pipe_layout, vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT, 0, bytes);
                d.cmd_draw(cmd, 4, 1, 0, 0);
            }
            d.cmd_end_rendering(cmd);
            barrier(d, cmd, target, 0, 1, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL, vk::ImageLayout::PRESENT_SRC_KHR,
                (vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE),
                (vk::PipelineStageFlags2::BOTTOM_OF_PIPE, vk::AccessFlags2::NONE));
            d.end_command_buffer(cmd).map_err(vkerr("end"))?;

            let wait = [vk::SemaphoreSubmitInfo::default().semaphore(self.acquire_sem).stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
            let signal = [vk::SemaphoreSubmitInfo::default().semaphore(self.present_sems[idx]).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            let submit = vk::SubmitInfo2::default().wait_semaphore_infos(&wait).signal_semaphore_infos(&signal).command_buffer_infos(&cmds);
            // The fence is reset only now: had the acquire failed above, a reset fence would
            // never be signalled and the next wait would hang.
            d.reset_fences(&[self.fence]).map_err(vkerr("reset fence"))?;
            d.queue_submit2(self.queue, &[submit], self.fence).map_err(vkerr("submit"))?;
            if let Some(s) = self.staging.as_mut() {
                s.used = 0;
            }

            let sems = [self.present_sems[idx]];
            let chains = [self.swapchain];
            let indices = [idx as u32];
            let t_sub = t0.elapsed();
            let pr = self.sw_fn.queue_present(self.queue, &vk::PresentInfoKHR::default().wait_semaphores(&sems).swapchains(&chains).image_indices(&indices));
            if TRACE.load(std::sync::atomic::Ordering::Relaxed) {
                let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
                crate::trace(&format!("    [gpu] fence {:.2} acquire {:.2} submit {:.2} present {:.2}", ms(t_fence), ms(t_acq), ms(t_sub), ms(t0.elapsed())));
            }
            match pr {
                Ok(suboptimal) => self.want_rebuild |= suboptimal,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.want_rebuild = true,
                Err(e) => return Err(format!("present: {e:?}")),
            }
            Ok(true)
        }
    }

    fn wait(&self) {
        // SAFETY: the fence is ours.
        unsafe {
            let _ = self.device.wait_for_fences(&[self.fence], true, u64::MAX);
        }
    }

    unsafe fn rebuild(&mut self) -> Result<(), String> {
        // SAFETY: the device is idle before the old swapchain's views go.
        unsafe {
            self.want_rebuild = false;
            let _ = self.device.device_wait_idle();
            let caps = self.surface_fn.get_physical_device_surface_capabilities(self.pdev, self.surface).map_err(vkerr("caps"))?;
            let extent = if caps.current_extent.width != u32::MAX {
                caps.current_extent
            } else {
                vk::Extent2D {
                    width: self.wanted.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: self.wanted.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            };
            self.extent = extent;
            if extent.width == 0 || extent.height == 0 {
                return Ok(());
            }
            let mut count = caps.min_image_count + 1;
            if caps.max_image_count > 0 {
                count = count.min(caps.max_image_count);
            }
            let old = self.swapchain;
            self.swapchain = self
                .sw_fn
                .create_swapchain(
                    &vk::SwapchainCreateInfoKHR::default()
                        .surface(self.surface)
                        .min_image_count(count)
                        .image_format(self.format)
                        .image_color_space(vk::ColorSpaceKHR::SRGB_NONLINEAR)
                        .image_extent(extent)
                        .image_array_layers(1)
                        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .pre_transform(caps.current_transform)
                        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                        .present_mode(self.present_mode)
                        .clipped(true)
                        .old_swapchain(old),
                    None,
                )
                .map_err(vkerr("vkCreateSwapchainKHR"))?;
            for v in self.views.drain(..) {
                self.device.destroy_image_view(v, None);
            }
            for s in self.present_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            if old != vk::SwapchainKHR::null() {
                self.sw_fn.destroy_swapchain(old, None);
            }
            self.images = self.sw_fn.get_swapchain_images(self.swapchain).map_err(vkerr("images"))?;
            for &img in &self.images {
                self.views.push(self.view(img, self.format, 1).map_err(vkerr("view"))?);
                self.present_sems.push(self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).map_err(vkerr("semaphore"))?);
            }
            Ok(())
        }
    }

    unsafe fn view(&self, image: vk::Image, format: vk::Format, mips: u32) -> ash::prelude::VkResult<vk::ImageView> {
        // SAFETY: `image` is live and of `format`.
        unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(range(0, mips)),
                None,
            )
        }
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags, avoid: vk::MemoryPropertyFlags) -> Option<u32> {
        let n = self.mem_props.memory_type_count as usize;
        let ok = |i: usize, avoid: vk::MemoryPropertyFlags| {
            let f = self.mem_props.memory_types[i].property_flags;
            bits & (1 << i) != 0 && f.contains(want) && !f.intersects(avoid)
        };
        (0..n).find(|&i| ok(i, avoid)).or_else(|| (0..n).find(|&i| ok(i, vk::MemoryPropertyFlags::empty()))).map(|i| i as u32)
    }

    fn make_texture(&self, w: u32, h: u32, mips: u32) -> Result<Texture, String> {
        // SAFETY: plain object creation; freed in `destroy_texture`.
        unsafe {
            let d = &self.device;
            let image = d.create_image(&image_info(w, h, mips), None).map_err(vkerr("image"))?;
            let req = d.get_image_memory_requirements(image);
            let ty = self.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL, vk::MemoryPropertyFlags::empty()).ok_or("no GPU memory type")?;
            let small = req.size <= SMALL_BLOCK && req.alignment <= SMALL_BLOCK;
            let free = if small { self.small.borrow().iter().position(|b| b.1 == ty) } else { None };
            let mem = match free {
                Some(i) => self.small.borrow_mut().swap_remove(i).0,
                None => match d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(if small { SMALL_BLOCK } else { req.size }).memory_type_index(ty), None) {
                    Ok(m) => m,
                    Err(e) => {
                        d.destroy_image(image, None);
                        return Err(format!("out of GPU memory: {e:?}"));
                    }
                },
            };
            let t = Texture { image, mem, pooled: small.then_some(ty), view: vk::ImageView::null(), w, h, mips };
            if let Err(e) = d.bind_image_memory(image, mem, 0) {
                self.destroy_texture(t);
                return Err(format!("bind: {e:?}"));
            }
            match self.view(image, vk::Format::B8G8R8A8_UNORM, mips) {
                Ok(view) => Ok(Texture { view, ..t }),
                Err(e) => {
                    self.destroy_texture(t);
                    Err(format!("view: {e:?}"))
                }
            }
        }
    }

    fn destroy_texture(&self, t: Texture) {
        // SAFETY: the caller waited for the fence; nothing uses the texture.
        unsafe {
            if t.view != vk::ImageView::null() {
                self.device.destroy_image_view(t.view, None);
            }
            self.device.destroy_image(t.image, None);
            match t.pooled {
                Some(ty) if self.small.borrow().len() < SMALL_KEPT => self.small.borrow_mut().push((t.mem, ty)),
                _ => self.device.free_memory(t.mem, None),
            }
        }
    }

    /// Hidden and trimmed: bring back what the next open's first frame writes, the start of the
    /// staging buffer (its pages cost milliseconds to fault back in on that frame). Only a
    /// small one is made here if there is none: a full-size one, or any GPU allocation, pulls
    /// tens of MB of driver memory into the working set.
    pub fn warm(&mut self) {
        self.wait();
        if self.staging.is_none() {
            match self.make_staging(WARM_STAGING) {
                Ok(s) => self.staging = Some(s),
                Err(_) => return,
            }
        }
        if let Some(s) = self.staging.as_ref() {
            // SAFETY: the mapping is `size` long and the GPU is done with it (fence waited).
            unsafe { std::ptr::write_bytes(s.ptr, 0, WARM_STAGING.min(s.size)) };
        }
    }

    /// Copy into the staging buffer, growing it if needed (keeping what this frame already put
    /// there). Returns the offset.
    fn stage(&mut self, px: &[u8]) -> Result<usize, String> {
        let used = self.staging.as_ref().map_or(0, |s| s.used);
        let need = used + px.len();
        if self.staging.as_ref().is_none_or(|s| s.size < need) {
            let size = need.next_power_of_two().max(KEPT_STAGING);
            let new = self.make_staging(size)?;
            if let Some(old) = self.staging.take() {
                // SAFETY: both mappings are live and at least `used` long.
                unsafe {
                    std::ptr::copy_nonoverlapping(old.ptr, new.ptr, old.used);
                    self.device.destroy_buffer(old.buf, None);
                    self.device.free_memory(old.mem, None);
                }
            }
            self.staging = Some(Staging { used, ..new });
        }
        let s = self.staging.as_mut().unwrap();
        // SAFETY: the mapping is `size` long and `need <= size`; the GPU is not reading it.
        unsafe { std::ptr::copy_nonoverlapping(px.as_ptr(), s.ptr.add(s.used), px.len()) };
        let at = s.used;
        s.used = (need + 15) & !15; // copies want 4-byte aligned offsets; 16 keeps it simple
        Ok(at)
    }

    fn make_staging(&self, size: usize) -> Result<Staging, String> {
        // SAFETY: plain creation; the memory is mapped once for its life.
        unsafe {
            let d = &self.device;
            let buf = d
                .create_buffer(&vk::BufferCreateInfo::default().size(size as u64).usage(vk::BufferUsageFlags::TRANSFER_SRC), None)
                .map_err(vkerr("staging"))?;
            let req = d.get_buffer_memory_requirements(buf);
            // Plain system memory first: on a resizable-BAR card the device-local visible heap
            // is small and precious, and a 100 MB photo would crowd it.
            let ty = self
                .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or("no host-visible memory")?;
            let mem = d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None).map_err(vkerr("staging memory"))?;
            d.bind_buffer_memory(buf, mem, 0).map_err(vkerr("bind"))?;
            let ptr = d.map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()).map_err(vkerr("map"))? as *mut u8;
            Ok(Staging { buf, mem, ptr, size, used: 0 })
        }
    }

    /// Staging → textures, and the mip chain by successive halving blits.
    fn record_uploads(&mut self) {
        let (d, cmd) = (&self.device, self.cmd);
        let Some(st) = self.staging.as_ref() else {
            self.uploads.clear();
            return;
        };
        for u in self.uploads.drain(..) {
            let Some(t) = self.tex[u.slot].as_ref() else { continue };
            // SAFETY: recording into our command buffer; the texture and buffer are live.
            unsafe {
                let top = (vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::NONE);
                let xfer_w = (vk::PipelineStageFlags2::TRANSFER, vk::AccessFlags2::TRANSFER_WRITE);
                let xfer_r = (vk::PipelineStageFlags2::TRANSFER, vk::AccessFlags2::TRANSFER_READ);
                let shader = (vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::SHADER_SAMPLED_READ);
                // UNDEFINED: every texel is overwritten, the old contents are not needed.
                barrier(d, cmd, t.image, 0, t.mips, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, top, xfer_w);
                let region = vk::BufferImageCopy {
                    buffer_offset: u.offset as u64,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: layers(0),
                    image_offset: vk::Offset3D::default(),
                    image_extent: vk::Extent3D { width: t.w, height: t.h, depth: 1 },
                };
                d.cmd_copy_buffer_to_image(cmd, st.buf, t.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
                let (mut w, mut h) = (t.w as i32, t.h as i32);
                for level in 1..t.mips {
                    barrier(d, cmd, t.image, level - 1, 1, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, xfer_w, xfer_r);
                    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
                    let blit = vk::ImageBlit {
                        src_subresource: layers(level - 1),
                        src_offsets: [vk::Offset3D::default(), vk::Offset3D { x: w, y: h, z: 1 }],
                        dst_subresource: layers(level),
                        dst_offsets: [vk::Offset3D::default(), vk::Offset3D { x: nw, y: nh, z: 1 }],
                    };
                    d.cmd_blit_image(cmd, t.image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, t.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[blit], vk::Filter::LINEAR);
                    (w, h) = (nw, nh);
                }
                if t.mips > 1 {
                    barrier(d, cmd, t.image, 0, t.mips - 1, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, xfer_r, shader);
                }
                barrier(d, cmd, t.image, t.mips - 1, 1, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, xfer_w, shader);
            }
        }
    }
}

fn range(base: u32, count: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: base, level_count: count, base_array_layer: 0, layer_count: 1 }
}

fn layers(level: u32) -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: level, base_array_layer: 0, layer_count: 1 }
}

#[allow(clippy::too_many_arguments)]
unsafe fn barrier(
    d: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    base: u32,
    count: u32,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src: (vk::PipelineStageFlags2, vk::AccessFlags2),
    dst: (vk::PipelineStageFlags2, vk::AccessFlags2),
) {
    let b = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src.0)
        .src_access_mask(src.1)
        .dst_stage_mask(dst.0)
        .dst_access_mask(dst.1)
        .old_layout(old)
        .new_layout(new)
        .image(image)
        .subresource_range(range(base, count))];
    // SAFETY: recording into a command buffer the caller began.
    unsafe { d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&b)) };
}

fn make_pipeline(d: &ash::Device, layout: vk::PipelineLayout, format: vk::Format) -> Result<vk::Pipeline, String> {
    let words = |b: &[u8]| -> Vec<u32> { b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect() };
    let vs = words(include_bytes!("../shaders/image.vert.spv"));
    let fs = words(include_bytes!("../shaders/image.frag.spv"));
    // SAFETY: the SPIR-V is ours, compiled by build.rs; modules are freed once the pipeline is made.
    unsafe {
        let vm = d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vs), None).map_err(vkerr("vs"))?;
        let fm = d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&fs), None).map_err(vkerr("fs"))?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vm).name(c"main"),
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fm).name(c"main"),
        ];
        let vi = vk::PipelineVertexInputStateCreateInfo::default();
        let ia = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
        let vp = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
        let rs = vk::PipelineRasterizationStateCreateInfo::default().polygon_mode(vk::PolygonMode::FILL).cull_mode(vk::CullModeFlags::NONE).line_width(1.0);
        let ms = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // Premultiplied "over": the photo writes alpha 1 and simply replaces, the overlays blend.
        let blend = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let ds = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let formats = [format];
        let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vi)
            .input_assembly_state(&ia)
            .viewport_state(&vp)
            .rasterization_state(&rs)
            .multisample_state(&ms)
            .color_blend_state(&cb)
            .dynamic_state(&ds)
            .layout(layout)
            .push_next(&mut rendering);
        let p = d.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None).map_err(|(_, e)| format!("pipeline: {e:?}"))?[0];
        d.destroy_shader_module(vm, None);
        d.destroy_shader_module(fm, None);
        Ok(p)
    }
}

fn image_info(w: u32, h: u32, mips: u32) -> vk::ImageCreateInfo<'static> {
    vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D { width: w, height: h, depth: 1 })
        .mip_levels(mips)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: the device is idle; everything is destroyed children first.
        unsafe {
            let _ = self.device.device_wait_idle();
            for t in std::mem::take(&mut self.tex).into_iter().flatten() {
                self.destroy_texture(t);
            }
            if let Some(s) = self.staging.take() {
                self.device.destroy_buffer(s.buf, None);
                self.device.free_memory(s.mem, None);
            }
            for (m, _) in self.small.take() {
                self.device.free_memory(m, None);
            }
            let d = &self.device;
            d.destroy_descriptor_pool(self.desc_pool, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.pipe_layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_command_pool(self.pool, None);
            d.destroy_fence(self.fence, None);
            d.destroy_semaphore(self.acquire_sem, None);
            for v in self.views.drain(..) {
                d.destroy_image_view(v, None);
            }
            for s in self.present_sems.drain(..) {
                d.destroy_semaphore(s, None);
            }
            self.sw_fn.destroy_swapchain(self.swapchain, None);
            d.destroy_device(None);
            self.surface_fn.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}
