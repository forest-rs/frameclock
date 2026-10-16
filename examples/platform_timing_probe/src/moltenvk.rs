// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Native window, Vulkan resource ownership, and measured host-clock conversion.

#![expect(
    unsafe_code,
    reason = "this explicit native probe owns the Vulkan handles and live AppKit view used by FFI"
)]

use std::ffi::CStr;
use std::time::{Duration as StdDuration, Instant};

use ash::{Entry, vk};
use frameclock::time::Timebase;
use frameclock::{
    DisplayTiming, Duration, FrameBeginResult, FrameDemand, FrameDriver, FrameOpportunity,
    FrameSubmission, FrameTick, HostTime, OutputId, PresentHints, PresentOutcome, SchedulerConfig,
};
use frameclock_vulkan::{
    Clock, ClockCorrelation, PresentId, PresentKey, PresentTracker, SwapchainId, TimeDomain,
};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::CGSize;
use objc2_quartz_core::{CACurrentMediaTime, CAMetalLayer};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

const FRAMES: u32 = 120;
// MoltenVK documents that its last one or two drawable callbacks can stall.
// A few extra presents release the tracked frames; their reports are ignored.
const DRAIN_FRAMES: u32 = 4;
const DOMAIN: TimeDomain = TimeDomain {
    swapchain: SwapchainId(1),
    clock: Clock::Presentation,
    id: 0,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

fn now() -> HostTime {
    // SAFETY: This process-wide monotonic clock has no preconditions.
    HostTime(unsafe { mach_absolute_time() })
}

fn timebase() -> Timebase {
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: info points to a valid, writable native result structure.
    assert_eq!(
        unsafe { mach_timebase_info(&mut info) },
        0,
        "Mach timebase query failed"
    );
    Timebase::new(info.numer, info.denom)
}

fn correlation(tb: Timebase) -> ClockCorrelation {
    let before = now();
    let media_ns = CACurrentMediaTime() * 1e9;
    let after = now();
    assert!(
        media_ns.is_finite() && media_ns >= 0.0 && media_ns < u64::MAX as f64,
        "invalid Core Animation clock reading"
    );
    #[expect(
        clippy::cast_possible_truncation,
        reason = "finite in-range positive media nanoseconds were checked above"
    )]
    let sample = media_ns as u64;
    let span = after.saturating_duration_since(before);
    let midpoint = before + Duration(span.ticks() / 2);
    // Include the whole CPU sampling bracket plus 10 us for floating-point
    // rounding and extrapolation over the short, awake-only experiment.
    let error_ns = tb.ticks_to_nanos(span.ticks()).saturating_add(10_000);
    ClockCorrelation::new(
        DOMAIN,
        sample,
        Timebase::NANOS,
        midpoint,
        tb,
        error_ns,
        midpoint + Duration::from_nanos(5_000_000_000, tb),
    )
    .expect("bounded media clock correlation")
}

struct Renderer {
    // Entry keeps the dynamic library loaded until every Vulkan object is gone.
    _entry: Entry,
    instance: ash::Instance,
    surface_api: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    device: ash::Device,
    queue: vk::Queue,
    swapchain_api: ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    timing: ash::google::display_timing::Device,
    wait: Option<ash::khr::present_wait::Device>,
    images: Vec<vk::Image>,
    initialized: Vec<bool>,
    pool: vk::CommandPool,
    command: vk::CommandBuffer,
    available: vk::Semaphore,
    finished: Vec<vk::Semaphore>,
    fence: vk::Fence,
    _layer: Retained<CAMetalLayer>,
    driver: FrameDriver,
    tracker: PresentTracker,
    tb: Timebase,
    early: u32,
    max_early_ns: u64,
    refresh: Duration,
    submitted: u32,
    resolved: u32,
    waits: u32,
    errors_ns: Vec<i128>,
    requested: Vec<u64>,
    started: Instant,
}

impl Renderer {
    fn new(window: &Window) -> Result<Self> {
        // SAFETY: Called from winit's main-thread resumed callback. The owned
        // window/layer outlive the surface. Handles are created in dependency
        // order, queue capabilities are queried, and each extension is enabled
        // only after enumeration. Startup failure exits this short-lived probe.
        unsafe {
            let path = std::env::args()
                .nth(1)
                .unwrap_or_else(|| "/opt/homebrew/lib/libMoltenVK.dylib".into());
            let entry = Entry::load_from(path)?;
            let supported_instance = entry.enumerate_instance_extension_properties(None)?;
            let has_instance = |name: &CStr| {
                supported_instance
                    .iter()
                    .any(|ext| CStr::from_ptr(ext.extension_name.as_ptr()) == name)
            };
            let mut extensions = vec![
                ash::khr::surface::NAME.as_ptr(),
                ash::ext::metal_surface::NAME.as_ptr(),
            ];
            let mut flags = vk::InstanceCreateFlags::empty();
            // The Vulkan loader advertises portability enumeration; a direct
            // MoltenVK entry point may not. Negotiate each independently.
            if has_instance(ash::khr::portability_enumeration::NAME) {
                extensions.push(ash::khr::portability_enumeration::NAME.as_ptr());
                flags = vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
            }
            let app = vk::ApplicationInfo::default()
                .application_name(c"frameclock timing probe")
                .api_version(vk::API_VERSION_1_1);
            let instance = entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app)
                    .flags(flags)
                    .enabled_extension_names(&extensions),
                None,
            )?;
            let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
                return Err("expected an AppKit window".into());
            };
            let view = handle.ns_view.cast::<AnyObject>().as_ref();
            let layer = CAMetalLayer::new();
            let size = window.inner_size();
            layer.setDrawableSize(CGSize::new(f64::from(size.width), f64::from(size.height)));
            layer.setContentsScale(window.scale_factor());
            let _: () = objc2::msg_send![view, setWantsLayer: true];
            let _: () = objc2::msg_send![view, setLayer: &*layer];
            let _: () = objc2::msg_send![&*layer, setDelegate: view];
            let metal_api = ash::ext::metal_surface::Instance::new(&entry, &instance);
            let surface = metal_api.create_metal_surface(
                &vk::MetalSurfaceCreateInfoEXT::default().layer(Retained::as_ptr(&layer).cast()),
                None,
            )?;
            let surface_api = ash::khr::surface::Instance::new(&entry, &instance);
            let (physical, family) = instance
                .enumerate_physical_devices()?
                .into_iter()
                .find_map(|physical| {
                    instance
                        .get_physical_device_queue_family_properties(physical)
                        .iter()
                        .enumerate()
                        .find_map(|(index, props)| {
                            let index = u32::try_from(index).ok()?;
                            (props.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                                && surface_api
                                    .get_physical_device_surface_support(physical, index, surface)
                                    .ok()?)
                            .then_some((physical, index))
                        })
                })
                .ok_or("no graphics/present queue")?;
            let props = instance.get_physical_device_properties(physical);
            println!(
                "device: {}",
                CStr::from_ptr(props.device_name.as_ptr()).to_string_lossy()
            );
            let supported = instance.enumerate_device_extension_properties(physical)?;
            let has = |name: &CStr| {
                supported
                    .iter()
                    .any(|ext| CStr::from_ptr(ext.extension_name.as_ptr()) == name)
            };
            for name in [
                ash::google::display_timing::NAME,
                ash::ext::calibrated_timestamps::NAME,
                ash::khr::calibrated_timestamps::NAME,
                ash::khr::present_id::NAME,
                ash::khr::present_wait::NAME,
                c"VK_KHR_present_wait2",
                c"VK_EXT_present_timing",
            ] {
                println!("{}: {}", name.to_string_lossy(), has(name));
            }
            if !has(ash::google::display_timing::NAME) {
                return Err("GOOGLE display timing is unavailable".into());
            }
            let mut enabled = vec![
                ash::khr::swapchain::NAME.as_ptr(),
                ash::google::display_timing::NAME.as_ptr(),
            ];
            if has(ash::khr::portability_subset::NAME) {
                enabled.push(ash::khr::portability_subset::NAME.as_ptr());
            }
            let calibrated = has(ash::ext::calibrated_timestamps::NAME);
            if calibrated {
                enabled.push(ash::ext::calibrated_timestamps::NAME.as_ptr());
            }
            let mut id_features = vk::PhysicalDevicePresentIdFeaturesKHR::default();
            let mut wait_features = vk::PhysicalDevicePresentWaitFeaturesKHR::default();
            let use_wait = if has(ash::khr::present_id::NAME) && has(ash::khr::present_wait::NAME) {
                instance.get_physical_device_features2(
                    physical,
                    &mut vk::PhysicalDeviceFeatures2::default()
                        .push_next(&mut id_features)
                        .push_next(&mut wait_features),
                );
                id_features.present_id == vk::TRUE && wait_features.present_wait == vk::TRUE
            } else {
                false
            };
            if use_wait {
                enabled.extend([
                    ash::khr::present_id::NAME.as_ptr(),
                    ash::khr::present_wait::NAME.as_ptr(),
                ]);
            }
            let priorities = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&priorities)];
            // Query builders leave links in the feature structures. Clear
            // them before constructing a separate device-creation chain;
            // appending an already-linked node twice would create a cycle.
            id_features.p_next = core::ptr::null_mut();
            wait_features.p_next = core::ptr::null_mut();
            let mut create = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(&enabled);
            if use_wait {
                create = create
                    .push_next(&mut id_features)
                    .push_next(&mut wait_features);
            }
            let device = instance.create_device(physical, &create, None)?;
            let queue = device.get_device_queue(family, 0);
            if calibrated {
                let api = ash::ext::calibrated_timestamps::Instance::new(&entry, &instance);
                let domains = api.get_physical_device_calibrateable_time_domains(physical)?;
                println!(
                    "calibratable domain IDs: {:?}",
                    domains
                        .iter()
                        .map(|domain| domain.as_raw())
                        .collect::<Vec<_>>()
                );
                let infos: Vec<_> = domains
                    .iter()
                    .map(|domain| vk::CalibratedTimestampInfoEXT::default().time_domain(*domain))
                    .collect();
                let api = ash::ext::calibrated_timestamps::Device::new(&instance, &device);
                let (samples, deviation) = api.get_calibrated_timestamps(&infos)?;
                println!("calibrated samples: {samples:?}; max deviation: {deviation} ns");
                // These samples are not the GOOGLE/Core Animation presentation
                // clock. That boundary uses separately bracketed media samples.
            }
            let caps = surface_api.get_physical_device_surface_capabilities(physical, surface)?;
            if !caps
                .supported_usage_flags
                .contains(vk::ImageUsageFlags::TRANSFER_DST)
            {
                return Err("surface cannot receive color clears".into());
            }
            let formats = surface_api.get_physical_device_surface_formats(physical, surface)?;
            let format = formats
                .iter()
                .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
                .unwrap_or(&formats[0]);
            let count = (caps.min_image_count + 1).min(if caps.max_image_count == 0 {
                u32::MAX
            } else {
                caps.max_image_count
            });
            let extent = if caps.current_extent.width != u32::MAX {
                caps.current_extent
            } else {
                vk::Extent2D {
                    width: size
                        .width
                        .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: size
                        .height
                        .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            };
            let alpha = [
                vk::CompositeAlphaFlagsKHR::OPAQUE,
                vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
                vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
                vk::CompositeAlphaFlagsKHR::INHERIT,
            ]
            .into_iter()
            .find(|flag| caps.supported_composite_alpha.contains(*flag))
            .ok_or("no composite alpha mode")?;
            let swapchain_api = ash::khr::swapchain::Device::new(&instance, &device);
            let swapchain = swapchain_api.create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .surface(surface)
                    .min_image_count(count)
                    .image_format(format.format)
                    .image_color_space(format.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(alpha)
                    .present_mode(vk::PresentModeKHR::FIFO)
                    .clipped(true),
                None,
            )?;
            let images = swapchain_api.get_swapchain_images(swapchain)?;
            let pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let command = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            let available = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
            let finished = images
                .iter()
                .map(|_| device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let fence = device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?;
            let timing = ash::google::display_timing::Device::new(&instance, &device);
            let refresh_ns = timing
                .get_refresh_cycle_duration(swapchain)?
                .refresh_duration;
            if refresh_ns == 0 {
                return Err("zero refresh interval".into());
            }
            let tb = timebase();
            let refresh = Duration::from_nanos(refresh_ns, tb);
            println!(
                "refresh: {refresh_ns} ns; Mach timebase: {}/{} ns/tick; present wait enabled: {use_wait}",
                tb.numer(),
                tb.denom()
            );
            let wait = use_wait.then(|| ash::khr::present_wait::Device::new(&instance, &device));
            Ok(Self {
                _entry: entry,
                instance,
                surface_api,
                surface,
                device: device.clone(),
                queue,
                swapchain_api,
                swapchain,
                timing,
                wait,
                initialized: vec![false; images.len()],
                images,
                pool,
                command,
                available,
                finished,
                fence,
                _layer: layer,
                driver: FrameDriver::new(SchedulerConfig::predictive(tb)),
                tracker: PresentTracker::new(FRAMES as usize),
                tb,
                early: 0,
                max_early_ns: 0,
                refresh,
                submitted: 0,
                resolved: 0,
                waits: 0,
                errors_ns: Vec::new(),
                requested: Vec::new(),
                started: Instant::now(),
            })
        }
    }

    fn poll_reports(&mut self) -> Result<()> {
        let clock = correlation(self.tb);
        // SAFETY: The device and swapchain are live. ash owns the output vector.
        for report in unsafe { self.timing.get_past_presentation_timing(self.swapchain)? } {
            if report.present_id > FRAMES {
                continue;
            }
            let expected_request = report
                .present_id
                .checked_sub(1)
                .and_then(|index| self.requested.get(usize::try_from(index).ok()?))
                .ok_or("feedback named an unsubmitted present")?;
            if report.desired_present_time != *expected_request {
                return Err("feedback changed the submitted target or present identity".into());
            }
            // Core Animation shows a timed present at the refresh nearest the
            // request, so one between refreshes can come up to half a refresh
            // early. Allow that, and 1 us for Metal's floating-point seconds;
            // count how often it happens.
            let early = report
                .desired_present_time
                .saturating_sub(report.actual_present_time);
            if early > 1_000 {
                self.early += 1;
                self.max_early_ns = self.max_early_ns.max(early);
            }
            if early > self.tb.ticks_to_nanos(self.refresh.ticks()) / 2 + 1_000 {
                return Err(format!(
                    "native present preceded requested time by more than half a refresh: actual {}, desired {}",
                    report.actual_present_time, report.desired_present_time
                )
                .into());
            }
            let key = PresentKey {
                swapchain: DOMAIN.swapchain,
                id: PresentId::Google(report.present_id),
            };
            let resolved = self
                .tracker
                .resolve_google(
                    key,
                    DOMAIN,
                    report.actual_present_time,
                    Some(clock),
                    now(),
                    Duration::from_nanos(1_000_000, self.tb),
                )
                .map_err(|error| format!("feedback correlation: {error:?}"))?;
            if !matches!(resolved.report.outcome, PresentOutcome::Presented(_)) {
                return Err(format!("unusable native presentation report: {resolved:?}").into());
            }
            let summary = self
                .driver
                .report_frame(resolved.frame_index, resolved.report)
                .ok_or("driver lost frame identity")?;
            let actual = summary
                .actual_present
                .ok_or("missing mapped actual present")?;
            let expected = summary.expected_present.ok_or("missing planned target")?;
            let magnitude = i128::from(
                self.tb
                    .ticks_to_nanos(actual.ticks().abs_diff(expected.ticks())),
            );
            self.errors_ns.push(if actual >= expected {
                magnitude
            } else {
                -magnitude
            });
            self.resolved += 1;
            if self.resolved == 1 || self.resolved.is_multiple_of(30) {
                println!(
                    "report {}: present {}, frame {}, expected {:?}, actual {:?}, error {} ns",
                    self.resolved,
                    report.present_id,
                    resolved.frame_index,
                    expected,
                    actual,
                    self.errors_ns.last().unwrap()
                );
            }
        }
        Ok(())
    }

    fn frame(&mut self) -> Result<bool> {
        self.poll_reports()?;
        if self.resolved == FRAMES {
            self.finish();
            return Ok(true);
        }
        if self.started.elapsed() > StdDuration::from_secs(12) {
            return Err(format!(
                "timed out: submitted {}, resolved {}, pending {}",
                self.submitted,
                self.resolved,
                self.tracker.pending_count()
            )
            .into());
        }
        if self.submitted == FRAMES + DRAIN_FRAMES {
            return Ok(false);
        }
        // SAFETY: One command buffer/fence serializes CPU command reuse. The
        // acquire semaphore is consumed before that fence signals. Each render-
        // finished semaphore belongs to an image and is reused only after that
        // same image is reacquired, so its previous present wait has completed.
        unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, 2_000_000_000)?;
            let tick_now = now();
            let target = tick_now + self.refresh;
            let opportunity = FrameOpportunity::new(
                FrameTick {
                    now: tick_now,
                    predicted_present: Some(target),
                    refresh_interval: Some(self.refresh),
                    output: OutputId(0),
                    prev_actual_present: None,
                },
                PresentHints::predictive(target, target - Duration::from_nanos(1_000_000, self.tb))
                    .starting_at_tick(),
                DisplayTiming::fixed(self.refresh),
            );
            let active = if self.submitted < FRAMES {
                self.driver.request(FrameDemand::ANIMATION);
                match self.driver.begin_frame(opportunity).result {
                    FrameBeginResult::Ready(frame) => Some(frame),
                    FrameBeginResult::Wait(_) | FrameBeginResult::Expired(_) => return Ok(false),
                    FrameBeginResult::Idle => return Err("animation demand was lost".into()),
                }
            } else {
                None
            };
            let (index, suboptimal) = self.swapchain_api.acquire_next_image(
                self.swapchain,
                2_000_000_000,
                self.available,
                vk::Fence::null(),
            )?;
            if suboptimal {
                return Err("swapchain changed during fixed-size probe".into());
            }
            let image_index = usize::try_from(index)?;
            let host_target = active
                .as_ref()
                // Ask for the slot, not the learned target, or the request
                // makes the learned latency come true.
                .and_then(|frame| frame.plan().requested_present())
                .unwrap_or(target);
            let raw_target = correlation(self.tb)
                .to_source(
                    DOMAIN,
                    host_target,
                    now(),
                    Duration::from_nanos(1_000_000, self.tb),
                )
                .map_err(|error| format!("target conversion: {error:?}"))?;
            self.device
                .reset_command_buffer(self.command, vk::CommandBufferResetFlags::empty())?;
            self.device.begin_command_buffer(
                self.command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let old_layout = if self.initialized[image_index] {
                vk::ImageLayout::PRESENT_SRC_KHR
            } else {
                vk::ImageLayout::UNDEFINED
            };
            let to_clear = vk::ImageMemoryBarrier::default()
                .image(self.images[image_index])
                .subresource_range(range)
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            self.device.cmd_pipeline_barrier(
                self.command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_clear],
            );
            let color = if (self.submitted / 30).is_multiple_of(2) {
                [0.06, 0.25, 0.16, 1.0]
            } else {
                [0.08, 0.16, 0.35, 1.0]
            };
            self.device.cmd_clear_color_image(
                self.command,
                self.images[image_index],
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: color },
                &[range],
            );
            let to_present = vk::ImageMemoryBarrier::default()
                .image(self.images[image_index])
                .subresource_range(range)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            self.device.cmd_pipeline_barrier(
                self.command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );
            self.device.end_command_buffer(self.command)?;
            self.device.reset_fences(&[self.fence])?;
            let commands = [self.command];
            let waits = [self.available];
            let stages = [vk::PipelineStageFlags::TRANSFER];
            let signals = [self.finished[image_index]];
            self.device.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(&commands)
                    .wait_semaphores(&waits)
                    .wait_dst_stage_mask(&stages)
                    .signal_semaphores(&signals)],
                self.fence,
            )?;
            self.initialized[image_index] = true;
            let present_id = self.submitted + 1;
            let times = [vk::PresentTimeGOOGLE::default()
                .present_id(present_id)
                .desired_present_time(raw_target)];
            let mut timing = vk::PresentTimesInfoGOOGLE::default().times(&times);
            let ids = [u64::from(present_id)];
            let mut id_info = vk::PresentIdKHR::default().present_ids(&ids);
            let swapchains = [self.swapchain];
            let indices = [index];
            let mut present = vk::PresentInfoKHR::default()
                .wait_semaphores(&signals)
                .swapchains(&swapchains)
                .image_indices(&indices)
                .push_next(&mut timing);
            if self.wait.is_some() {
                present = present.push_next(&mut id_info);
            }
            if self.swapchain_api.queue_present(self.queue, &present)? {
                return Err("presentation became suboptimal".into());
            }
            self.requested.push(raw_target);
            if let Some(frame) = active {
                let key = PresentKey {
                    swapchain: DOMAIN.swapchain,
                    id: PresentId::Google(present_id),
                };
                self.tracker
                    .register(key, frame.plan().frame_index)
                    .map_err(|error| format!("registration: {error:?}"))?;
                let _ = self
                    .driver
                    .submit_frame(frame, FrameSubmission::reported(now()));
            }
            self.submitted += 1;
            if let Some(wait) = &self.wait {
                // Wait completion is backpressure only. It never produces an
                // actual presentation timestamp or resolves a tracked frame.
                wait.wait_for_present(self.swapchain, u64::from(present_id), 2_000_000_000)?;
                self.waits += 1;
            }
        }
        Ok(false)
    }

    fn finish(&mut self) {
        self.errors_ns.sort_unstable();
        println!(
            "PASS: {} scheduled frames, {} real GOOGLE reports, {} successful KHR present waits, {} pending tracked frames",
            FRAMES,
            self.resolved,
            self.waits,
            self.tracker.pending_count()
        );
        println!(
            "actual - planned: min {} ns, median {} ns, max {} ns",
            self.errors_ns[0],
            self.errors_ns[self.errors_ns.len() / 2],
            self.errors_ns[self.errors_ns.len() - 1]
        );
        println!(
            "shown before the requested time: {} frames, by at most {} ns",
            self.early, self.max_early_ns
        );
        println!(
            "learned present latency {} ns",
            self.tb
                .ticks_to_nanos(self.driver.scheduler().present_latency_ticks())
        );
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: No other thread uses these handles. Quiesce GPU work, then
        // destroy resources before their parents; the window/layer remain live.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_semaphore(self.available, None);
            for semaphore in &self.finished {
                self.device.destroy_semaphore(*semaphore, None);
            }
            self.device.destroy_command_pool(self.pool, None);
            self.swapchain_api.destroy_swapchain(self.swapchain, None);
            self.device.destroy_device(None);
            self.surface_api.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

#[derive(Default)]
struct Probe {
    // Drop renderer before its native window.
    renderer: Option<Renderer>,
    window: Option<Window>,
    error: Option<String>,
    wake: Option<Instant>,
}

impl ApplicationHandler for Probe {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let result = (|| -> Result<()> {
            let window = event_loop.create_window(
                Window::default_attributes()
                    .with_title("frameclock / MoltenVK timing probe")
                    .with_inner_size(winit::dpi::LogicalSize::new(480.0, 280.0))
                    .with_resizable(false),
            )?;
            self.renderer = Some(Renderer::new(&window)?);
            window.request_redraw();
            self.window = Some(window);
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(error.to_string());
            event_loop.exit();
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, _cause: StartCause) {
        if self.wake.is_some_and(|wake| Instant::now() >= wake) {
            self.wake = None;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if event == WindowEvent::CloseRequested {
            self.error = Some("probe closed before completion".into());
            event_loop.exit();
        }
        if event != WindowEvent::RedrawRequested {
            return;
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        match renderer.frame() {
            Ok(true) => event_loop.exit(),
            Ok(false) => {
                // The native timing request controls display. This host timer
                // bounds CPU production; acquire and optional present waits
                // provide the native backpressure.
                let wake = Instant::now()
                    + StdDuration::from_nanos(renderer.tb.ticks_to_nanos(renderer.refresh.ticks()));
                self.wake = Some(wake);
                event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
            }
            Err(error) => {
                self.error = Some(error.to_string());
                event_loop.exit();
            }
        }
    }
}

pub(super) fn run() {
    let event_loop = EventLoop::new().expect("create AppKit event loop");
    let mut probe = Probe::default();
    event_loop
        .run_app(&mut probe)
        .expect("run AppKit event loop");
    // Explicitly release Vulkan while winit still owns the window.
    drop(probe.renderer.take());
    if let Some(error) = probe.error {
        eprintln!("FAIL: {error}");
        std::process::exit(1);
    }
}
