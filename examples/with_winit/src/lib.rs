// Copyright 2022 the Velato Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! With Winit

// The following lints are part of the Linebender standard set,
// but resolving them has been deferred for now.
// Feel free to send a PR that solves one or more of these.
#![allow(
    missing_docs,
    clippy::wildcard_imports,
    clippy::use_self,
    clippy::missing_errors_doc,
    clippy::shadow_unrelated,
    clippy::cast_possible_truncation,
    clippy::allow_attributes,
    clippy::allow_attributes_without_reason
)]

use instant::Instant;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use kurbo::{Affine, Vec2};
use peniko::Color;
use scenes::{RobotoText, SceneParams, SceneSet};

#[cfg(feature = "use_vello")]
use std::num::NonZeroUsize;
#[cfg(feature = "use_vello")]
use vello::low_level::BumpAllocators;
#[cfg(feature = "use_vello")]
use vello::util::{RenderContext, RenderSurface};
#[cfg(feature = "use_vello")]
use vello::{AaConfig, Renderer, RendererOptions, Scene, wgpu};

#[cfg(feature = "use_ekrano")]
use ekrano::Scene;

use winit::event_loop::{EventLoop, EventLoopBuilder};
use winit::window::Window;

#[cfg(all(
    feature = "use_vello",
    not(any(target_arch = "wasm32", target_os = "android"))
))]
mod hot_reload;
mod multi_touch;
mod stats;

#[derive(Parser, Debug)]
#[command(about, long_about = None, bin_name="cargo run -p with_winit --")]
struct Args {
    /// Which scene (index) to start on
    /// Switch between scenes with left and right arrow keys
    #[arg(long)]
    scene: Option<i32>,
    #[command(flatten)]
    args: scenes::Arguments,
    #[arg(long)]
    /// Whether to use CPU shaders
    use_cpu: bool,
    /// Whether to force initialising the shaders serially (rather than
    /// spawning threads) This has no effect on wasm, and defaults to 1 on
    /// macOS for performance reasons
    ///
    /// Use `0` for an automatic choice
    #[arg(long, default_value_t=default_threads())]
    num_init_threads: usize,
}

fn default_threads() -> usize {
    #[cfg(target_os = "macos")]
    return 1;
    #[cfg(not(target_os = "macos"))]
    return 0;
}

#[cfg(feature = "use_vello")]
struct RenderState<'s> {
    // SAFETY: We MUST drop the surface before the `window`, so the fields
    // must be in this order
    surface: RenderSurface<'s>,
    window: Arc<Window>,
}

#[cfg(feature = "use_vello")]
fn run(
    event_loop: EventLoop<UserEvent>,
    args: Args,
    mut scenes: SceneSet,
    render_cx: RenderContext,
    #[cfg(target_arch = "wasm32")] render_state: RenderState<'_>,
) {
    use winit::event::*;
    use winit::event_loop::ControlFlow;
    use winit::keyboard::*;
    let mut renderers: Vec<Option<Renderer>> = vec![];
    #[cfg(not(target_arch = "wasm32"))]
    let mut render_cx = render_cx;
    #[cfg(not(target_arch = "wasm32"))]
    let mut render_state = None::<RenderState<'_>>;
    let use_cpu = args.use_cpu;
    // The design of `RenderContext` forces delayed renderer initialisation to
    // not work on wasm, as WASM futures effectively must be 'static.
    // Otherwise, this could work by sending the result to event_loop.proxy
    // instead of blocking
    #[cfg(target_arch = "wasm32")]
    let mut render_state = {
        renderers.resize_with(render_cx.devices.len(), || None);
        let id = render_state.surface.dev_id;
        let renderer = Renderer::new(
            &render_cx.devices[id].device,
            RendererOptions {
                use_cpu,
                antialiasing_support: vello::AaSupport::all(),
                // We currently initialise on one thread on WASM, but mark this here
                // anyway
                num_init_threads: NonZeroUsize::new(1),
                pipeline_cache: None,
            },
        )
        .expect("Could create renderer");
        renderers[id] = Some(renderer);
        Some(render_state)
    };
    // Whilst suspended, we drop `render_state`, but need to keep the same
    // window. If render_state exists, we must store the window in it, to
    // maintain drop order
    #[cfg(not(target_arch = "wasm32"))]
    let mut cached_window = None;

    let mut scene = Scene::new();
    let mut fragment = Scene::new();
    let mut simple_text = RobotoText::new();
    let mut stats = stats::Stats::new();
    let mut stats_shown = true;
    // Currently not updated in wasm builds
    #[allow(unused_mut)]
    let mut scene_complexity: Option<BumpAllocators> = None;
    let mut complexity_shown = false;
    let mut vsync_on = true;

    const AA_CONFIGS: [AaConfig; 3] = [AaConfig::Area, AaConfig::Msaa8, AaConfig::Msaa16];
    // We allow cycling through AA configs in either direction, so use a signed
    // index
    let mut aa_config_ix: i32 = 0;

    let mut frame_start_time = Instant::now();
    let start = Instant::now();

    let mut touch_state = multi_touch::TouchState::new();
    // navigation_fingers are fingers which are used in the navigation 'zone' at
    // the bottom of the screen. This ensures that one press on the screen
    // doesn't have multiple actions
    let mut navigation_fingers = HashSet::new();
    let mut transform = Affine::IDENTITY;
    let mut mouse_down = false;
    let mut prior_position: Option<Vec2> = None;
    // We allow looping left and right through the scenes, so use a signed index
    let mut scene_ix: i32 = 0;
    let mut complexity: usize = 0;
    if let Some(set_scene) = args.scene {
        scene_ix = set_scene;
    }
    let mut prev_scene_ix = scene_ix - 1;
    let mut modifiers = ModifiersState::default();
    event_loop
        .run(move |event, event_loop| match event {
            Event::WindowEvent {
                ref event,
                window_id,
            } => {
                let Some(render_state) = &mut render_state else {
                    return;
                };
                if render_state.window.id() != window_id {
                    return;
                }
                match event {
                    WindowEvent::CloseRequested => event_loop.exit(),
                    WindowEvent::ModifiersChanged(m) => modifiers = m.state(),
                    WindowEvent::KeyboardInput { event, .. } => {
                        if event.state == ElementState::Pressed {
                            match event.logical_key.as_ref() {
                                Key::Named(NamedKey::ArrowLeft) => {
                                    scene_ix = scene_ix.saturating_sub(1);
                                }
                                Key::Named(NamedKey::ArrowRight) => {
                                    scene_ix = scene_ix.saturating_add(1);
                                }
                                Key::Named(NamedKey::ArrowUp) => complexity += 1,
                                Key::Named(NamedKey::ArrowDown) => {
                                    complexity = complexity.saturating_sub(1);
                                }
                                Key::Named(NamedKey::Space) => {
                                    transform = Affine::IDENTITY;
                                }
                                Key::Character(char) => {
                                    // TODO: Have a more principled way of handling modifiers on keypress
                                    // see e.g. https://xi.zulipchat.com/#narrow/stream/351333-glazier/topic/Keyboard.20shortcuts
                                    let char = char.to_lowercase();
                                    match char.as_str() {
                                        "q" | "e" => {
                                            if let Some(prior_position) = prior_position {
                                                let is_clockwise = char == "e";
                                                let angle = if is_clockwise { -0.05 } else { 0.05 };
                                                transform = Affine::translate(prior_position)
                                                    * Affine::rotate(angle)
                                                    * Affine::translate(-prior_position)
                                                    * transform;
                                            }
                                        }
                                        "s" => {
                                            stats_shown = !stats_shown;
                                        }
                                        "d" => {
                                            complexity_shown = !complexity_shown;
                                        }
                                        "c" => {
                                            stats.clear_min_and_max();
                                        }
                                        "m" => {
                                            aa_config_ix = if modifiers.shift_key() {
                                                aa_config_ix.saturating_sub(1)
                                            } else {
                                                aa_config_ix.saturating_add(1)
                                            };
                                        }
                                        "v" => {
                                            vsync_on = !vsync_on;
                                            render_cx.set_present_mode(
                                                &mut render_state.surface,
                                                if vsync_on {
                                                    wgpu::PresentMode::AutoVsync
                                                } else {
                                                    wgpu::PresentMode::AutoNoVsync
                                                },
                                            );
                                        }
                                        _ => {}
                                    }
                                }
                                Key::Named(NamedKey::Escape) => event_loop.exit(),
                                _ => {}
                            }
                        }
                    }
                    WindowEvent::Touch(touch) => {
                        match touch.phase {
                            TouchPhase::Started => {
                                // We reserve the bottom third of the screen for navigation
                                // This also prevents strange effects whilst using the navigation gestures on Android
                                // TODO: How do we know what the client area is? Winit seems to just give us the
                                // full screen
                                // TODO: Render a display of the navigation regions. We don't do
                                // this currently because we haven't researched how to determine when we're
                                // in a touch context (i.e. Windows/Linux/MacOS with a touch screen could
                                // also be using mouse/keyboard controls)
                                // Note that winit's rendering is y-down
                                if touch.location.y
                                    > render_state.surface.config.height as f64 * 2. / 3.
                                {
                                    navigation_fingers.insert(touch.id);
                                    // The left third of the navigation zone navigates backwards
                                    if touch.location.x
                                        < render_state.surface.config.width as f64 / 3.
                                    {
                                        scene_ix = scene_ix.saturating_sub(1);
                                    } else if touch.location.x
                                        > 2. * render_state.surface.config.width as f64 / 3.
                                    {
                                        scene_ix = scene_ix.saturating_add(1);
                                    }
                                }
                            }
                            TouchPhase::Ended | TouchPhase::Cancelled => {
                                // We intentionally ignore the result here
                                navigation_fingers.remove(&touch.id);
                            }
                            TouchPhase::Moved => (),
                        }
                        // See documentation on navigation_fingers
                        if !navigation_fingers.contains(&touch.id) {
                            touch_state.add_event(touch);
                        }
                    }
                    WindowEvent::Resized(size) => {
                        render_cx.resize_surface(
                            &mut render_state.surface,
                            size.width,
                            size.height,
                        );
                        render_state.window.request_redraw();
                    }
                    WindowEvent::MouseInput { state, button, .. } => {
                        if button == &MouseButton::Left {
                            mouse_down = state == &ElementState::Pressed;
                        }
                    }
                    WindowEvent::MouseWheel { delta, .. } => {
                        const BASE: f64 = 1.05;
                        const PIXELS_PER_LINE: f64 = 20.0;

                        if let Some(prior_position) = prior_position {
                            let exponent = if let MouseScrollDelta::PixelDelta(delta) = delta {
                                delta.y / PIXELS_PER_LINE
                            } else if let MouseScrollDelta::LineDelta(_, y) = delta {
                                *y as f64
                            } else {
                                0.0
                            };
                            transform = Affine::translate(prior_position)
                                * Affine::scale(BASE.powf(exponent))
                                * Affine::translate(-prior_position)
                                * transform;
                        } else {
                            eprintln!(
                                "Scrolling without mouse in window; this shouldn't be possible"
                            );
                        }
                    }
                    WindowEvent::CursorLeft { .. } => {
                        prior_position = None;
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        let position = Vec2::new(position.x, position.y);
                        if mouse_down && let Some(prior) = prior_position {
                            transform = Affine::translate(position - prior) * transform;
                        }
                        prior_position = Some(position);
                    }
                    WindowEvent::RedrawRequested => {
                        let width = render_state.surface.config.width;
                        let height = render_state.surface.config.height;
                        let device_handle = &render_cx.devices[render_state.surface.dev_id];
                        let snapshot = stats.snapshot();

                        // Allow looping forever
                        scene_ix = scene_ix.rem_euclid(scenes.scenes.len() as i32);
                        aa_config_ix = aa_config_ix.rem_euclid(AA_CONFIGS.len() as i32);

                        let example_scene = &mut scenes.scenes[scene_ix as usize];
                        if prev_scene_ix != scene_ix {
                            transform = Affine::IDENTITY;
                            prev_scene_ix = scene_ix;
                            render_state
                                .window
                                .set_title(&format!("Vello demo - {}", example_scene.config.name));
                        }
                        fragment.reset();
                        let mut scene_params = SceneParams {
                            time: start.elapsed().as_secs_f64(),
                            text: &mut simple_text,
                            resolution: None,
                            base_color: None,
                            interactive: true,
                            complexity,
                        };
                        example_scene
                            .function
                            .render(&mut fragment, &mut scene_params);

                        // If the user specifies a base color in the CLI we use that. Otherwise we use any
                        // color specified by the scene. The default is black.
                        let base_color = args
                            .args
                            .base_color
                            .or(scene_params.base_color)
                            .unwrap_or(Color::BLACK);
                        let antialiasing_method = AA_CONFIGS[aa_config_ix as usize];
                        let render_params = vello::RenderParams {
                            base_color,
                            width,
                            height,
                            antialiasing_method,
                        };
                        scene.reset();
                        let mut transform = transform;
                        if let Some(resolution) = scene_params.resolution {
                            // Automatically scale the rendering to fill as much of the window as possible
                            let factor = Vec2::new(width as f64, height as f64);
                            let scale_factor =
                                (factor.x / resolution.x).min(factor.y / resolution.y);
                            transform *= Affine::scale(scale_factor);
                        }
                        scene.append(&fragment, Some(transform));
                        if stats_shown {
                            snapshot.draw_layer(
                                &mut scene,
                                scene_params.text,
                                width as f64,
                                height as f64,
                                stats.samples(),
                                complexity_shown.then_some(scene_complexity).flatten(),
                                vsync_on,
                                antialiasing_method,
                            );
                        }
                        let surface = &render_state.surface;
                        // Note: we don't run the async/"robust" pipeline, as
                        // it requires more async wiring for the readback. See
                        // [#vello > async on wasm](https://xi.zulipchat.com/#narrow/channel/197075-vello/topic/async.20on.20wasm/with/396685264)
                        #[allow(deprecated)]
                        // #[expect(deprecated, reason = "This deprecation is not targeted at us.")] // Our MSRV is too low to use `expect`
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            scene_complexity = vello::util::block_on_wgpu(
                                &device_handle.device,
                                renderers[render_state.surface.dev_id]
                                    .as_mut()
                                    .unwrap()
                                    .render_to_texture_async(
                                        &device_handle.device,
                                        &device_handle.queue,
                                        &scene,
                                        &surface.target_view,
                                        &render_params,
                                        vello::low_level::DebugLayers::none(),
                                    ),
                            )
                            .expect("failed to render to surface");
                        }
                        // Note: in the wasm case, we're currently not running the robust
                        // pipeline, as it requires more async wiring for the readback.
                        #[cfg(target_arch = "wasm32")]
                        renderers[render_state.surface.dev_id]
                            .as_mut()
                            .unwrap()
                            .render_to_texture(
                                &device_handle.device,
                                &device_handle.queue,
                                &scene,
                                &surface.target_view,
                                &render_params,
                            )
                            .expect("failed to render to surface");

                        let surface_texture = surface
                            .surface
                            .get_current_texture()
                            .expect("failed to get surface texture");

                        let mut encoder = device_handle.device.create_command_encoder(
                            &wgpu::CommandEncoderDescriptor {
                                label: Some("Surface Blit"),
                            },
                        );

                        surface.blitter.copy(
                            &device_handle.device,
                            &mut encoder,
                            &surface.target_view,
                            &surface_texture
                                .texture
                                .create_view(&wgpu::TextureViewDescriptor::default()),
                        );

                        device_handle.queue.submit([encoder.finish()]);
                        surface_texture.present();
                        device_handle.device.poll(wgpu::PollType::Poll).unwrap();

                        let new_time = Instant::now();
                        stats.add_sample(stats::Sample {
                            frame_time_us: (new_time - frame_start_time).as_micros() as u64,
                        });
                        frame_start_time = new_time;
                    }
                    _ => {}
                }
            }
            Event::AboutToWait => {
                touch_state.end_frame();
                let touch_info = touch_state.info();
                if let Some(touch_info) = touch_info {
                    let centre = Vec2::new(touch_info.zoom_centre.x, touch_info.zoom_centre.y);
                    transform = Affine::translate(touch_info.translation_delta)
                        * Affine::translate(centre)
                        * Affine::scale(touch_info.zoom_delta)
                        * Affine::rotate(touch_info.rotation_delta)
                        * Affine::translate(-centre)
                        * transform;
                }

                if let Some(render_state) = &mut render_state {
                    render_state.window.request_redraw();
                }
            }
            Event::UserEvent(event) => match event {
                #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
                UserEvent::HotReload => {
                    let Some(render_state) = &mut render_state else {
                        return;
                    };
                    let device_handle = &render_cx.devices[render_state.surface.dev_id];
                    eprintln!("==============\nReloading shaders");
                    let start = Instant::now();
                    let result = renderers[render_state.surface.dev_id]
                        .as_mut()
                        .unwrap()
                        .reload_shaders(&device_handle.device);
                    // We know that the only async here (`pop_error_scope`) is actually sync, so blocking is fine
                    match pollster::block_on(result) {
                        Ok(_) => eprintln!("Reloading took {:?}", start.elapsed()),
                        Err(e) => eprintln!("Failed to reload shaders because of {e}"),
                    }
                }
            },
            Event::Suspended => {
                eprintln!("Suspending");
                #[cfg(not(target_arch = "wasm32"))]
                // When we suspend, we need to remove the `wgpu` Surface
                if let Some(render_state) = render_state.take() {
                    cached_window = Some(render_state.window);
                }
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Event::Resumed => {
                #[cfg(target_arch = "wasm32")]
                {}
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let None = render_state else { return };
                    let window = cached_window
                        .take()
                        .unwrap_or_else(|| create_window(event_loop));
                    let size = window.inner_size();
                    let surface_future = render_cx.create_surface(
                        window.clone(),
                        size.width,
                        size.height,
                        wgpu::PresentMode::AutoVsync,
                    );
                    // We need to block here, in case a Suspended event appeared
                    let surface =
                        pollster::block_on(surface_future).expect("Error creating surface");
                    render_state = {
                        let render_state = RenderState { window, surface };
                        renderers.resize_with(render_cx.devices.len(), || None);
                        let id = render_state.surface.dev_id;
                        renderers[id].get_or_insert_with(|| {
                            let start = Instant::now();
                            let renderer = Renderer::new(
                                &render_cx.devices[id].device,
                                RendererOptions {
                                    use_cpu,
                                    antialiasing_support: vello::AaSupport::all(),
                                    num_init_threads: NonZeroUsize::new(args.num_init_threads),
                                    pipeline_cache: None,
                                },
                            )
                            .expect("Could create renderer");
                            eprintln!("Creating renderer {id} took {:?}", start.elapsed());
                            renderer
                        });
                        Some(render_state)
                    };
                    event_loop.set_control_flow(ControlFlow::Poll);
                }
            }
            _ => {}
        })
        .expect("run to completion");
}

#[cfg(feature = "use_vello")]
fn create_window(event_loop: &winit::event_loop::EventLoopWindowTarget<UserEvent>) -> Arc<Window> {
    use winit::dpi::LogicalSize;
    use winit::window::WindowBuilder;
    Arc::new(
        WindowBuilder::new()
            .with_inner_size(LogicalSize::new(1044, 800))
            .with_resizable(true)
            .with_title("Vello demo")
            .build(event_loop)
            .unwrap(),
    )
}

#[cfg(feature = "use_vello")]
#[derive(Debug)]
enum UserEvent {
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    HotReload,
}

/// # Panics
/// Can panic.
#[cfg(feature = "use_vello")]
pub fn main() -> Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    env_logger::init();
    let args = Args::parse();
    let scenes = args.args.select_scene_set(Args::command)?;
    if let Some(scenes) = scenes {
        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build()?;
        #[allow(unused_mut)]
        let mut render_cx = RenderContext::new();
        #[cfg(not(target_arch = "wasm32"))]
        {
            #[cfg(not(target_os = "android"))]
            let proxy = event_loop.create_proxy();
            #[cfg(not(target_os = "android"))]
            let _keep = hot_reload::hot_reload(move || {
                proxy.send_event(UserEvent::HotReload).ok().map(drop)
            });

            run(event_loop, args, scenes, render_cx);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Ekrano backend
// ---------------------------------------------------------------------------
#[cfg(feature = "use_ekrano")]
fn create_ekrano_window(event_loop: &winit::event_loop::EventLoopWindowTarget<()>) -> Arc<Window> {
    use winit::dpi::LogicalSize;
    use winit::window::WindowBuilder;
    Arc::new(
        WindowBuilder::new()
            .with_inner_size(LogicalSize::new(1044, 800))
            .with_resizable(true)
            .with_title("Ekrano demo")
            .build(event_loop)
            .unwrap(),
    )
}

#[cfg(feature = "use_ekrano")]
fn run_ekrano(event_loop: EventLoop<()>, args: Args, mut scenes: SceneSet) {
    use ekrano::{GoldyRenderer, RenderParams};
    use goldy::{DeviceType, Instance, PresentMode, Surface, SurfaceConfig};
    use winit::event::*;
    use winit::event_loop::ControlFlow;
    use winit::keyboard::*;

    // Force a backtrace on any panic so we can diagnose crashes from user input
    // without requiring RUST_BACKTRACE to be set in the environment.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: set_var is only unsafe on some platforms (multi-threaded env),
        // but we're before any thread spawns here.
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("\n===== PANIC in run_ekrano =====");
        eprintln!("{info}");
        eprintln!("Backtrace:\n{}", std::backtrace::Backtrace::force_capture());
        eprintln!("================================\n");
        prev_hook(info);
    }));

    let instance = Instance::new().expect("Failed to create Goldy instance");
    let device = instance
        .create_device(DeviceType::DiscreteGpu)
        .or_else(|_| instance.create_device(DeviceType::IntegratedGpu))
        .or_else(|_| instance.create_device(DeviceType::Other))
        .expect("No GPU device found");

    let start_create = Instant::now();
    let mut renderer = GoldyRenderer::new(&device).expect("Failed to create ekrano renderer");
    eprintln!("Creating ekrano renderer took {:?}", start_create.elapsed());

    let mut window: Option<Arc<Window>> = None;
    let mut surface: Option<Surface> = None;
    let mut vsync = true;

    let mut scene = Scene::new();
    let mut fragment = Scene::new();
    let mut simple_text = RobotoText::new();
    let mut stats = stats::Stats::new();
    let mut stats_shown = true;

    let mut frame_start_time = Instant::now();
    let start = Instant::now();

    let mut touch_state = multi_touch::TouchState::new();
    let _navigation_fingers: HashSet<u64> = HashSet::new();
    let mut transform = Affine::IDENTITY;
    let mut mouse_down = false;
    let mut prior_position: Option<Vec2> = None;
    let mut _modifiers = winit::keyboard::ModifiersState::default();
    let mut device_lost = false;
    let mut scene_ix: i32 = 0;
    let mut complexity: usize = 0;
    let mut complexity_shown = false;
    if let Some(set_scene) = args.scene {
        scene_ix = set_scene;
    }
    let mut prev_scene_ix = scene_ix - 1;

    event_loop
        .run(move |event, event_loop| match event {
            Event::WindowEvent {
                ref event,
                window_id,
            } => {
                let Some(win) = &window else { return };
                if win.id() != window_id {
                    return;
                }
                match event {
                    WindowEvent::CloseRequested => {
                        event_loop.exit();
                    }
                    WindowEvent::ModifiersChanged(m) => {
                        _modifiers = m.state();
                    }
                    WindowEvent::KeyboardInput { event, .. } => {
                        if event.state == ElementState::Pressed {
                            match event.logical_key.as_ref() {
                                Key::Named(NamedKey::ArrowLeft) => {
                                    scene_ix = scene_ix.saturating_sub(1);
                                }
                                Key::Named(NamedKey::ArrowRight) => {
                                    scene_ix = scene_ix.saturating_add(1);
                                }
                                Key::Named(NamedKey::ArrowUp) => complexity += 1,
                                Key::Named(NamedKey::ArrowDown) => {
                                    complexity = complexity.saturating_sub(1);
                                }
                                Key::Named(NamedKey::Space) => {
                                    transform = Affine::IDENTITY;
                                }
                                Key::Named(NamedKey::Escape) => {
                                    event_loop.exit();
                                }
                                Key::Character(char) => {
                                    let char = char.to_lowercase();
                                    match char.as_str() {
                                        "q" | "e" => {
                                            if let Some(prior_position) = prior_position {
                                                let is_clockwise = char == "e";
                                                let angle = if is_clockwise { -0.05 } else { 0.05 };
                                                transform = Affine::translate(prior_position)
                                                    * Affine::rotate(angle)
                                                    * Affine::translate(-prior_position)
                                                    * transform;
                                            }
                                        }
                                        "s" => stats_shown = !stats_shown,
                                        "c" => stats.clear_min_and_max(),
                                        "d" => complexity_shown = !complexity_shown,
                                        "m" => {
                                            eprintln!(
                                                "AA method switching not available in ekrano mode"
                                            );
                                        }
                                        "v" => {
                                            // Ignore auto-repeat to avoid flipping vsync hundreds of
                                            // times a second when the user holds the key.
                                            if event.repeat {
                                                // no-op
                                            } else if let Some(surf) = surface.as_mut() {
                                                vsync = !vsync;
                                                let mode = if vsync {
                                                    PresentMode::Fifo
                                                } else {
                                                    PresentMode::Immediate
                                                };
                                                match surf.set_present_mode(mode) {
                                                    Ok(()) => eprintln!(
                                                        "Vsync: {} (present mode: {:?})",
                                                        if vsync { "ON" } else { "OFF" },
                                                        mode
                                                    ),
                                                    Err(e) => {
                                                        eprintln!("Failed to set present mode: {e}")
                                                    }
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    WindowEvent::Resized(size) => {
                        if let Some(surf) = &mut surface {
                            let _ = surf.resize(size.width.max(1), size.height.max(1));
                        }
                        if let Some(win) = &window {
                            win.request_redraw();
                        }
                    }
                    WindowEvent::MouseInput { state, button, .. } => {
                        if button == &MouseButton::Left {
                            mouse_down = state == &ElementState::Pressed;
                        }
                    }
                    WindowEvent::MouseWheel { delta, .. } => {
                        const BASE: f64 = 1.05;
                        const PIXELS_PER_LINE: f64 = 20.0;
                        if let Some(prior_position) = prior_position {
                            let exponent = if let MouseScrollDelta::PixelDelta(delta) = delta {
                                delta.y / PIXELS_PER_LINE
                            } else if let MouseScrollDelta::LineDelta(_, y) = delta {
                                *y as f64
                            } else {
                                0.0
                            };
                            transform = Affine::translate(prior_position)
                                * Affine::scale(BASE.powf(exponent))
                                * Affine::translate(-prior_position)
                                * transform;
                        }
                    }
                    WindowEvent::Touch(touch) => {
                        touch_state.add_event(touch);
                    }
                    WindowEvent::CursorLeft { .. } => {
                        prior_position = None;
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        let position = Vec2::new(position.x, position.y);
                        if mouse_down && let Some(prior) = prior_position {
                            transform = Affine::translate(position - prior) * transform;
                        }
                        prior_position = Some(position);
                    }
                    WindowEvent::RedrawRequested => {
                        if device_lost {
                            return;
                        }
                        let Some(win) = &window else { return };
                        let Some(surf) = surface.as_mut() else { return };
                        let (width, height) = surf.size();
                        if width == 0 || height == 0 {
                            return;
                        }
                        let snapshot = stats.snapshot();

                        scene_ix = scene_ix.rem_euclid(scenes.scenes.len() as i32);
                        let example_scene = &mut scenes.scenes[scene_ix as usize];
                        if prev_scene_ix != scene_ix {
                            transform = Affine::IDENTITY;
                            prev_scene_ix = scene_ix;
                            win.set_title(&format!("Ekrano demo - {}", example_scene.config.name));
                        }
                        fragment.reset();
                        let mut scene_params = SceneParams {
                            time: start.elapsed().as_secs_f64(),
                            text: &mut simple_text,
                            resolution: None,
                            base_color: None,
                            interactive: true,
                            complexity,
                        };
                        example_scene
                            .function
                            .render(&mut fragment, &mut scene_params);

                        let base_color = args
                            .args
                            .base_color
                            .or(scene_params.base_color)
                            .unwrap_or(Color::BLACK);
                        let render_params = RenderParams {
                            base_color,
                            width,
                            height,
                            antialiasing_method: ekrano::AaConfig::Area,
                        };
                        scene.reset();
                        let mut transform = transform;
                        if let Some(resolution) = scene_params.resolution {
                            let factor = Vec2::new(width as f64, height as f64);
                            let scale_factor =
                                (factor.x / resolution.x).min(factor.y / resolution.y);
                            transform *= Affine::scale(scale_factor);
                        }
                        scene.append(&fragment, Some(transform));
                        if stats_shown {
                            snapshot.draw_layer(
                                &mut scene,
                                &mut simple_text,
                                width as f64,
                                height as f64,
                                stats.samples(),
                                None,
                                vsync,
                                ekrano::AaConfig::Area,
                            );
                        }

                        let frame = match surf.acquire() {
                            Ok(f) => f,
                            Err(e) => {
                                eprintln!("surface.acquire error: {e}");
                                let error_text = e.to_string();
                                if error_text.contains("Failed to wait for frame fence")
                                    || error_text.contains("DEVICE_LOST")
                                    || error_text.contains("device lost")
                                {
                                    device_lost = true;
                                    event_loop.exit();
                                }
                                return;
                            }
                        };
                        let frame_tex = frame.texture().clone();
                        let render_result =
                            renderer.render_to_texture(&device, &scene, &frame_tex, &render_params);
                        // Always drop the borrowed texture handle and present the
                        // frame, even on render error: otherwise the drawable stays
                        // retained by the Metal layer and `nextDrawable` starves
                        // after 3 frames, turning a recoverable render error into
                        // an unrecoverable `surface.acquire` hang.
                        drop(frame_tex);
                        if let Err(e) = frame.present() {
                            eprintln!("surface.present error: {e}");
                        }
                        match render_result {
                            Ok(stats) if stats.bump_retries > 0 => {
                                eprintln!(
                                    "[BUMP] bump allocator reallocated {} time(s) this frame",
                                    stats.bump_retries,
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("Render error: {e}");
                                // `GPU device is lost` means a prior wait_fence timed out
                                // and the device is permanently wedged. Every subsequent
                                // frame will fail the same way; exit so the user isn't
                                // flooded with identical errors.
                                if e.to_string().contains("GPU device is lost") {
                                    eprintln!("GPU is wedged — exiting");
                                    event_loop.exit();
                                }
                                return;
                            }
                        }

                        let new_time = Instant::now();
                        stats.add_sample(stats::Sample {
                            frame_time_us: (new_time - frame_start_time).as_micros() as u64,
                        });
                        frame_start_time = new_time;
                    }
                    _ => {}
                }
            }
            Event::AboutToWait => {
                touch_state.end_frame();
                let touch_info = touch_state.info();
                if let Some(touch_info) = touch_info {
                    let centre = Vec2::new(touch_info.zoom_centre.x, touch_info.zoom_centre.y);
                    transform = Affine::translate(touch_info.translation_delta)
                        * Affine::translate(centre)
                        * Affine::scale(touch_info.zoom_delta)
                        * Affine::rotate(touch_info.rotation_delta)
                        * Affine::translate(-centre)
                        * transform;
                }
                if let Some(win) = &window {
                    win.request_redraw();
                }
            }
            Event::Resumed => {
                let win = create_ekrano_window(event_loop);
                let initial_mode = if vsync {
                    PresentMode::Fifo
                } else {
                    PresentMode::Immediate
                };
                let surf = Surface::new_with_config(
                    &device,
                    win.as_ref(),
                    SurfaceConfig {
                        present_mode: initial_mode,
                        depth_format: None,
                    },
                )
                .expect("Failed to create goldy surface");
                window = Some(win);
                surface = Some(surf);
                event_loop.set_control_flow(ControlFlow::Poll);
            }
            Event::Suspended => {
                surface = None;
                window = None;
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Event::LoopExiting => {
                // When device is lost, drop the surface (and window) here so that
                // surface cleanup runs while the device is still in the devices map.
                // This ensures all Vulkan child objects are destroyed before
                // vkDestroyDevice is called, avoiding VUID-vkDestroyDevice-device-05137.
                if device_lost {
                    surface = None;
                    window = None;
                }
            }
            _ => {}
        })
        .expect("run to completion");
}

/// # Panics
/// Can panic.
#[cfg(feature = "use_ekrano")]
pub fn main() -> Result<()> {
    // Goldy logs its GPU diagnostics (timeouts, completion-handler errors,
    // descriptor encode paths) via `tracing`. `env_logger` only understands
    // the `log` crate, so those events never reach the terminal. Installing
    // a tracing-subscriber fmt layer that honors `RUST_LOG` surfaces them.
    // Default to `warn` so a plain `cargo run` produces clean stdout suitable
    // for FPS comparisons against upstream vello. Goldy / ekrano startup
    // tracing and per-frame perf heartbeats are still reachable via
    // `RUST_LOG=goldy=info,ekrano=debug` or similar.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    // Route `log::*` records into the tracing subscriber so legacy callers
    // (velato itself, scenes, etc.) interleave with goldy's tracing output
    // rather than disappearing.
    let _ = tracing_log::LogTracer::init();
    let args = Args::parse();
    let scenes = args.args.select_scene_set(Args::command)?;
    if let Some(scenes) = scenes {
        let event_loop = EventLoopBuilder::<()>::with_user_event().build()?;
        run_ekrano(event_loop, args, scenes);
    }
    Ok(())
}
