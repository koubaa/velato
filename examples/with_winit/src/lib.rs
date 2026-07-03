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

#[cfg(all(feature = "use_vello", feature = "use_ekrano"))]
compile_error!(
    "Enable only one renderer backend: default for vello, or \
     `--no-default-features --features use_ekrano` for ekrano"
);
#[cfg(feature = "use_vello")]
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
    /// Disable vsync (use Immediate / AutoNoVsync present mode)
    #[arg(long)]
    no_vsync: bool,
    /// Auto-exit after this many seconds
    #[arg(long)]
    timeout_secs: Option<u64>,
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
    let mut vsync_on = !args.no_vsync;
    let auto_exit_deadline = args
        .timeout_secs
        .map(|s| Instant::now() + std::time::Duration::from_secs(s));

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

                if let Some(deadline) = auto_exit_deadline {
                    if Instant::now() >= deadline {
                        event_loop.exit();
                        return;
                    }
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
                        if vsync_on {
                            wgpu::PresentMode::AutoVsync
                        } else {
                            wgpu::PresentMode::AutoNoVsync
                        },
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

/// Writes a small JSON file identifying the active backend, PID, and timestamp.
/// Only runs when `BACKEND_DUMP_DIR` is explicitly set in the environment;
/// silently skipped otherwise so non-developer runs are unaffected.
fn dump_backend_info(backend: &str) {
    use std::io::Write;
    let Ok(dir) = std::env::var("BACKEND_DUMP_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{backend}_backend.json"));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    match std::fs::File::create(&path) {
        Ok(mut f) => {
            let _ = writeln!(
                f,
                r#"{{"backend":"{backend}","pid":{},"unix_ts":{ts}}}"#,
                std::process::id(),
            );
            println!("[with_winit] Backend dump written to {}", path.display());
        }
        Err(e) => {
            println!(
                "[with_winit] FAILED to write backend dump to {}: {e}",
                path.display()
            );
        }
    }
}

/// # Panics
/// Can panic.
#[cfg(all(feature = "use_vello", not(feature = "use_ekrano")))]
pub fn main() -> Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    env_logger::init();
    dump_backend_info("vello");
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

/// Wraps either a [`goldy::Surface`] (Classic path) or a [`goldy::SwapchainPool`]
/// (Scheme path) so both can flow through the same command channel.
///
/// Deleted with the Classic (TaskGraph) backend in Phase 6.
///
/// Scheme present pipeline depth and speculative-acquire policy depend on the
/// backend:
///
/// - **Metal / DX12**: depth=2, speculative_acquire=false. Two in-flight drawables match
///   the physical swapchain ring (`MAX_FRAMES_IN_FLIGHT = 2` on DX12). The render thread
///   stashes the next drawable during the overlap phase via [`goldy::SwapchainPool::try_early_acquire`]
///   after dispatching a present token; submit takes the stash via `resolve_present_slot`.
///   Legacy TID_PRESENT speculative acquire remains opt-in via `GOLDY_ENABLE_SPECULATIVE_ACQUIRE=1`.
/// - **Vulkan**: depth=1, speculative_acquire=false. Flip-model acquire before
///   the previous present has released the image produces an ACQUIRE RACE and
///   exhausts the depth gate immediately.
#[cfg(feature = "use_ekrano")]
fn scheme_pool_options(
    backend_type: goldy::BackendType,
    config: goldy::SurfaceConfig,
) -> goldy::SwapchainPoolOptions {
    match backend_type {
        goldy::BackendType::Metal | goldy::BackendType::Dx12 => goldy::SwapchainPoolOptions {
            depth: 2,
            config,
            speculative_acquire: false,
        },
        goldy::BackendType::Vulkan => goldy::SwapchainPoolOptions {
            depth: 1,
            config,
            speculative_acquire: false,
        },
    }
}

#[cfg(feature = "use_ekrano")]
enum SurfaceOrPool {
    Classic(goldy::Surface),
    Scheme(goldy::SwapchainPool),
}

#[cfg(feature = "use_ekrano")]
enum RenderCmd {
    SurfaceCreated(SurfaceOrPool),
    SurfaceDropped,
    TransformSet(Affine),
    SceneDelta(i32),
    ComplexityDelta(i32),
    ToggleStats,
    ClearStats,
    ToggleVsync,
    Resize(u32, u32),
    Shutdown,
}

#[cfg(feature = "use_ekrano")]
struct InputState {
    transform: Affine,
    scene_ix: i32,
    complexity: usize,
    stats_shown: bool,
    vsync: bool,
    width: u32,
    height: u32,
    start: Instant,
}

#[cfg(feature = "use_ekrano")]
fn is_device_lost_error(err: &impl std::fmt::Display) -> bool {
    let s = err.to_string();
    s.contains("0x887A0005")
        || s.contains("DEVICE_REMOVED")
        || s.contains("device removed")
        || s.contains("DEVICE_LOST")
        || s.contains("device lost")
        || s.contains("Failed to wait for frame fence")
        || s.contains("GPU device is lost")
}

#[cfg(feature = "use_ekrano")]
fn is_swapchain_out_of_date_error(err: &impl std::fmt::Display) -> bool {
    let s = err.to_string();
    s.contains("out of date") || s.contains("OUT_OF_DATE")
}

#[cfg(feature = "use_ekrano")]
fn apply_render_cmd(
    cmd: RenderCmd,
    surface_or_pool: &mut Option<SurfaceOrPool>,
    input: &mut InputState,
    stats: &Arc<std::sync::Mutex<stats::Stats>>,
) -> bool {
    use goldy::PresentMode;

    match cmd {
        RenderCmd::TransformSet(transform) => input.transform = transform,
        RenderCmd::SceneDelta(delta) => input.scene_ix = input.scene_ix.saturating_add(delta),
        RenderCmd::ComplexityDelta(delta) => {
            if delta >= 0 {
                input.complexity = input.complexity.saturating_add(delta as usize);
            } else {
                input.complexity = input.complexity.saturating_sub((-delta) as usize);
            }
        }
        RenderCmd::ToggleStats => input.stats_shown = !input.stats_shown,
        RenderCmd::ClearStats => {
            if let Ok(mut stats) = stats.lock() {
                stats.clear_min_and_max();
            }
        }
        RenderCmd::ToggleVsync => {
            input.vsync = !input.vsync;
            let mode = if input.vsync { PresentMode::Fifo } else { PresentMode::Immediate };
            match surface_or_pool.as_mut() {
                None => return true,
                Some(SurfaceOrPool::Classic(s)) => {
                    match s.set_present_mode(mode) {
                        Ok(()) => eprintln!("Vsync: {} (present mode: {:?})", if input.vsync { "ON" } else { "OFF" }, mode),
                        Err(e) => eprintln!("Failed to set present mode: {e}"),
                    }
                }
                Some(SurfaceOrPool::Scheme(p)) => {
                    match p.set_present_mode(mode) {
                        Ok(()) => eprintln!("Vsync: {} (present mode: {:?})", if input.vsync { "ON" } else { "OFF" }, mode),
                        Err(e) => eprintln!("Failed to set present mode: {e}"),
                    }
                }
            }
        }
        RenderCmd::Resize(width, height) => {
            let width = width.max(1);
            let height = height.max(1);
            // Do NOT call surface.resize() here. A deferred resize is applied once at the
            // end of drain_commands (see surface_dirty flag) so that a burst of per-pixel
            // events during a smooth window drag produces at most one swapchain rebuild —
            // not one per event.
            input.width = width;
            input.height = height;
        }
        RenderCmd::SurfaceCreated(sop) => {
            let (width, height) = match &sop {
                SurfaceOrPool::Classic(s) => s.size(),
                SurfaceOrPool::Scheme(p) => p.size(),
            };
            input.width = width;
            input.height = height;
            *surface_or_pool = Some(sop);
        }
        RenderCmd::SurfaceDropped => {
            *surface_or_pool = None;
        }
        RenderCmd::Shutdown => {
            return false;
        }
    }
    true
}

#[cfg(feature = "use_ekrano")]
enum DrainResult {
    Shutdown,
    Idle,
    SurfaceDirty,
}

#[cfg(feature = "use_ekrano")]
fn drain_commands_without_resize(
    cmd_rx: &std::sync::mpsc::Receiver<RenderCmd>,
    surface_or_pool: &mut Option<SurfaceOrPool>,
    input: &mut InputState,
    stats: &Arc<std::sync::Mutex<stats::Stats>>,
) -> DrainResult {
    let _tz = goldy::tracy_zone!("velato.drain_commands");
    // Set when a command that can change surface dimensions or present mode is drained.
    // The deferred resize fires exactly once when this is true, batching any burst of
    // per-pixel Resize events from a smooth window drag into a single swapchain rebuild.
    let mut surface_dirty = false;
    loop {
        match cmd_rx.try_recv() {
            Ok(cmd) => {
                surface_dirty |= matches!(cmd, RenderCmd::Resize(..) | RenderCmd::ToggleVsync);
                if !apply_render_cmd(cmd, surface_or_pool, input, stats) {
                    return DrainResult::Shutdown;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return DrainResult::Shutdown;
            }
        }
    }

    if surface_dirty {
        DrainResult::SurfaceDirty
    } else {
        DrainResult::Idle
    }
}

/// Rebuild the scheme swapchain at the latest requested dimensions.
///
/// Used both proactively on [`DrainResult::SurfaceDirty`] (after present ack) and
/// reactively when submit/acquire reports OUT_OF_DATE.
#[cfg(feature = "use_ekrano")]
fn scheme_swapchain_resize(
    surface_or_pool: &mut Option<SurfaceOrPool>,
    input: &mut InputState,
    renderer: &mut ekrano::GoldyRenderer,
    ctx: &goldy::Context,
    device_lost: &std::sync::atomic::AtomicBool,
    sent_present_tokens: u64,
    reason: &str,
) -> bool {
    let Some(SurfaceOrPool::Scheme(p)) = surface_or_pool.as_mut() else {
        return true;
    };
    p.sync_before_rebuild(ctx, sent_present_tokens);
    match p.resize(input.width, input.height) {
        Ok(()) => {
            input.width = p.width();
            input.height = p.height();
            renderer.invalidate_swapchain_retention();
            true
        }
        Err(e) => {
            if reason == "reactive" {
                eprintln!("reactive scheme resize error: {e}");
            }
            if is_device_lost_error(&e) {
                device_lost.store(true, std::sync::atomic::Ordering::Relaxed);
                false
            } else {
                true
            }
        }
    }
}

/// Rebuild the scheme swapchain after Vulkan reports OUT_OF_DATE. Waits for the
/// in-flight present round-trip first so FIFO scheduled presents finish before
/// swapchain recreation.
#[cfg(feature = "use_ekrano")]
fn reactive_scheme_resize(
    surface_or_pool: &mut Option<SurfaceOrPool>,
    input: &mut InputState,
    present_in_flight: &mut bool,
    present_sent_seq: u64,
    presenter: &Presenter,
    device_lost: &std::sync::atomic::AtomicBool,
    renderer: &mut ekrano::GoldyRenderer,
) -> bool {
    if *present_in_flight {
        if let Some(SurfaceOrPool::Scheme(pool)) = surface_or_pool.as_ref() {
            let _tz = goldy::tracy_zone!("velato.wait_present_completed");
            pool.wait_present_completed(present_sent_seq);
        } else {
            let _tz = goldy::tracy_zone!("velato.wait_present_ack");
            if !presenter.wait_for_present_ack() {
                return false;
            }
        }
        *present_in_flight = false;
    }
    scheme_swapchain_resize(
        surface_or_pool,
        input,
        renderer,
        &renderer.submission_context(),
        device_lost,
        present_sent_seq,
        "reactive",
    )
}

#[cfg(feature = "use_ekrano")]
fn apply_deferred_surface_resize(
    surface_or_pool: &mut Option<SurfaceOrPool>,
    input: &mut InputState,
    renderer: &mut ekrano::GoldyRenderer,
    device_lost: &std::sync::atomic::AtomicBool,
    sent_present_tokens: u64,
) -> bool {
    // A present-mode change (ToggleVsync) requires an immediate swapchain rebuild before
    // the next frame. The surface/pool may clamp the requested dimensions to its capability
    // limits, so sync input.width/height to the real swapchain afterwards.
    match surface_or_pool.as_mut() {
        Some(SurfaceOrPool::Classic(s)) => {
            let _ = s.resize(input.width, input.height);
            input.width = s.width();
            input.height = s.height();
            true
        }
        Some(SurfaceOrPool::Scheme(_)) => {
            scheme_swapchain_resize(
                surface_or_pool,
                input,
                renderer,
                &renderer.submission_context(),
                device_lost,
                sent_present_tokens,
                "proactive",
            )
        }
        None => true,
    }
}

#[cfg(feature = "use_ekrano")]
fn scheme_render_dimensions(
    surface_or_pool: Option<&SurfaceOrPool>,
    input: &InputState,
) -> (u32, u32) {
    match surface_or_pool {
        Some(SurfaceOrPool::Scheme(p)) => (p.width(), p.height()),
        _ => (input.width, input.height),
    }
}

#[cfg(feature = "use_ekrano")]
fn build_ekrano_scene(
    scene: &mut ekrano::Scene,
    fragment: &mut ekrano::Scene,
    scenes: &mut SceneSet,
    input: &InputState,
    simple_text: &mut RobotoText,
    stats: &stats::Stats,
    base_color: Option<Color>,
    render_width: u32,
    render_height: u32,
) -> ekrano::RenderParams {
    use ekrano::RenderParams;

    let scene_ix = input.scene_ix.rem_euclid(scenes.scenes.len() as i32);
    let example_scene = &mut scenes.scenes[scene_ix as usize];
    fragment.reset();
    let mut scene_params = SceneParams {
        time: input.start.elapsed().as_secs_f64(),
        text: simple_text,
        resolution: None,
        base_color: None,
        interactive: true,
        complexity: input.complexity,
    };
    example_scene.function.render(fragment, &mut scene_params);

    let resolved_base_color = base_color
        .or(scene_params.base_color)
        .unwrap_or(Color::BLACK);
    let render_params = RenderParams {
        base_color: resolved_base_color,
        width: render_width,
        height: render_height,
        antialiasing_method: ekrano::AaConfig::Area,
        robust: false,
    };

    let mut transform = input.transform;
    if let Some(resolution) = scene_params.resolution {
        let factor = Vec2::new(input.width as f64, input.height as f64);
        let scale_factor = (factor.x / resolution.x).min(factor.y / resolution.y);
        transform *= Affine::scale(scale_factor);
    }

    scene.reset();
    scene.append(fragment, Some(transform));
    if input.stats_shown {
        stats.snapshot().draw_layer(
            scene,
            simple_text,
            input.width as f64,
            input.height as f64,
            stats.samples(),
            None,
            input.vsync,
            ekrano::AaConfig::Area,
        );
    }

    render_params
}

#[cfg(feature = "use_ekrano")]
enum PresentPayload {
    Classic(goldy::Frame),
    Scheme(ekrano::PresentToken),
}

#[cfg(feature = "use_ekrano")]
enum Presenter {
    /// Synchronous present on the render thread. Retained for easy A/B testing;
    /// `Presenter::new` currently always builds [`Self::Threaded`].
    #[allow(dead_code)]
    Inline,
    Threaded {
        tx: std::sync::mpsc::SyncSender<PresentPayload>,
        /// Receives one `()` per presented frame, so TID_RENDER can wait for
        /// present-N to complete before acquiring image N+1.
        ack_rx: std::sync::mpsc::Receiver<()>,
        handle: Option<std::thread::JoinHandle<()>>,
    },
}

#[cfg(feature = "use_ekrano")]
impl Presenter {
    fn new(device_lost: Arc<std::sync::atomic::AtomicBool>) -> Self {
        // Capacity 0 would be a rendezvous; 1 lets TID_RENDER stay one
        // frame ahead of TID_PRESENT while still bounding the pipeline.
        let (tx, rx) = std::sync::mpsc::sync_channel::<PresentPayload>(1);
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let dl = Arc::clone(&device_lost);
        let handle = std::thread::Builder::new()
            .name("TID_PRESENT".into())
            .spawn(move || {
                // NOTE (macOS): this thread has no implicit autorelease pool.
                // The Metal backend manages drawable/command-buffer lifetimes
                // explicitly (retain on acquire, release on present), so this is
                // expected to be safe; if Tracy shows per-frame growth or
                // nextDrawable stalls, wrap each present in an `@autoreleasepool`.
                while let Ok(payload) = rx.recv() {
                    let _tz = goldy::tracy_zone!("frame.present.async");
                    let present_err: Option<String> = match payload {
                        PresentPayload::Classic(frame) => {
                            let err = frame.present().err().map(|e| e.to_string());
                            let _ = ack_tx.send(());
                            err
                        }
                        // Scheme path: lifecycle is pool counters (present_began / present_completed).
                        // Do not send on ack_tx — velato no longer drains it on this path and a
                        // capacity-1 sync_channel would wedge TID_PRESENT after the second frame.
                        PresentPayload::Scheme(token) => {
                            token.present_scanout().err().map(|e| e.to_string())
                        }
                    };
                    if let Some(e) = present_err {
                        eprintln!("present error (TID_PRESENT): {e}");
                        if is_device_lost_error(&e) {
                            dl.store(true, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                    }
                }
})
            .expect("spawn TID_PRESENT");
        Self::Threaded {
            tx,
            ack_rx,
            handle: Some(handle),
        }
    }

    /// Dispatch a frame to present. Returns `false` if the render loop should exit.
    ///
    /// For `Threaded`, this is non-blocking when the channel has capacity (depth 1).
    /// The next acquire/record/submit may overlap TID_PRESENT; backpressure is on
    /// the bounded channel and acquire-side frame-latency waits.
    fn send_payload(&self, payload: PresentPayload, device_lost: &std::sync::atomic::AtomicBool) -> bool {
        use std::sync::atomic::Ordering;

        let _tz = goldy::tracy_zone!("velato.present_send");
        match self {
            Self::Inline => {
                let present_err: Option<String> = match payload {
                    PresentPayload::Classic(frame) => frame.present().err().map(|e| e.to_string()),
                    PresentPayload::Scheme(token) => token.present().err().map(|e| e.to_string()),
                };
                if let Some(e) = present_err {
                    eprintln!("present error: {e}");
                    if is_device_lost_error(&e) {
                        device_lost.store(true, Ordering::Relaxed);
                        return false;
                    }
                }
                true
            }
            Self::Threaded { tx, .. } => tx.send(payload).is_ok(),
        }
    }

    fn send_frame(&self, frame: goldy::Frame, device_lost: &std::sync::atomic::AtomicBool) -> bool {
        self.send_payload(PresentPayload::Classic(frame), device_lost)
    }

    fn send_present_token(
        &self,
        token: ekrano::PresentToken,
        device_lost: &std::sync::atomic::AtomicBool,
    ) -> bool {
        self.send_payload(PresentPayload::Scheme(token), device_lost)
    }

    /// Block until TID_PRESENT has finished presenting the previously sent frame.
    ///
    /// Required before swapchain rebuilds (`resize` / present-mode changes) because
    /// both paths take the backend mutex and rebuild live swapchain images.
    ///
    /// On `Inline` this is a no-op (present already completed synchronously).
    fn wait_for_present_ack(&self) -> bool {
        match self {
            Self::Inline => true,
            Self::Threaded { ack_rx, .. } => ack_rx.recv().is_ok(),
        }
    }

    /// Non-blocking check whether TID_PRESENT finished the in-flight frame.
    fn try_present_ack(&self) -> bool {
        match self {
            Self::Inline => true,
            Self::Threaded { ack_rx, .. } => ack_rx.try_recv().is_ok(),
        }
    }

    fn shutdown(self) {
match self {
            Self::Inline => {
}
            Self::Threaded { tx, mut handle, .. } => {
drop(tx);
                if let Some(handle) = handle.take() {
let join_start = Instant::now();
                    let _ = handle.join();
}
            }
        }
}
}

// ---------------------------------------------------------------------------
// Render-thread helpers
// ---------------------------------------------------------------------------

/// Outcome of a single-frame rendering step.
///
/// Returned by helpers that can fail in two distinct ways, letting the main
/// render loop stay readable without inline `break`/`continue` branches.
#[cfg(feature = "use_ekrano")]
enum RenderStep<T> {
    /// Step produced a value; processing continues normally.
    Ok(T),
    /// Transient failure (e.g. prepare error); skip this frame and retry.
    SkipFrame {
        /// When true, the swapchain should be rebuilt at the latest requested size.
        out_of_date: bool,
    },
    /// Fatal condition (device lost or channel closed); exit the render loop.
    Shutdown,
}

/// Blocks the render thread until a [`goldy::Surface`] is available, processing
/// any incoming [`RenderCmd`]s in the meantime.
///
/// Returns `false` if the thread should exit entirely (channel disconnected or
/// `Shutdown` command received).
#[cfg(feature = "use_ekrano")]
fn block_until_surface(
    cmd_rx: &std::sync::mpsc::Receiver<RenderCmd>,
    surface_or_pool: &mut Option<SurfaceOrPool>,
    input: &mut InputState,
    stats: &Arc<std::sync::Mutex<stats::Stats>>,
) -> bool {
    while surface_or_pool.is_none() {
        let _tz = goldy::tracy_zone!("velato.wait_surface");
        match cmd_rx.recv() {
            Ok(cmd) => {
                if !apply_render_cmd(cmd, surface_or_pool, input, stats) {
                    return false;
                }
            }
            Err(_) => {
                return false;
            }
        }
    }
    true
}

/// Drains pending UI commands, then applies deferred resize when needed.
///
/// **Ordering guarantee (resize):** when a drained command requires swapchain
/// rebuild (`Resize` / `ToggleVsync`), `wait_for_present_ack` must complete
/// before `apply_deferred_surface_resize` because both `resize` and
/// `present_frame` lock the same backend mutex and rebuild/read live swapchain
/// images.
///
/// **Ordering guarantee (normal path):** `wait_for_present_ack` is also
/// required before every acquire, because `GetCurrentBackBufferIndex` must
/// observe the previous `swapchain.Present()` call to rotate to the next
/// backbuffer.  Without this gate two consecutive acquires read the same
/// backbuffer index and produce two FrameTokens for the same image; both
/// presents then copy to the same backbuffer concurrently, corrupting GPU
/// state.
///
/// CPU scene-building overlap is preserved: `build_overlap_stash` (Phase 4)
/// runs *after* `send_present_token` and *before* this function, so the
/// render thread still overlaps scene construction with TID_PRESENT's GPU
/// copy/present.  Acquire/record/submit overlap requires N-backed swapchain
/// slots and is deferred to future work.
///
/// Returns `false` if the render loop should exit.
#[cfg(feature = "use_ekrano")]
fn sync_present_and_drain(
    present_in_flight: &mut bool,
    present_sent_seq: u64,
    presenter: &Presenter,
    cmd_rx: &std::sync::mpsc::Receiver<RenderCmd>,
    surface_or_pool: &mut Option<SurfaceOrPool>,
    input: &mut InputState,
    stats: &Arc<std::sync::Mutex<stats::Stats>>,
    renderer: &mut ekrano::GoldyRenderer,
    device_lost: &std::sync::atomic::AtomicBool,
    scheme_stash_clear: &mut bool,
    scheme_skip_early_acquire: &mut bool,
) -> bool {
    let drain = drain_commands_without_resize(cmd_rx, surface_or_pool, input, stats);
    match drain {
        DrainResult::Shutdown => return false,
        DrainResult::SurfaceDirty => {
            if *present_in_flight {
                if let Some(SurfaceOrPool::Scheme(pool)) = surface_or_pool.as_ref() {
                    let _tz = goldy::tracy_zone!("velato.wait_present_completed");
                    pool.wait_present_completed(present_sent_seq);
                } else {
                    let _tz = goldy::tracy_zone!("velato.wait_present_ack");
                    if !presenter.wait_for_present_ack() {
                        return false;
                    }
                }
                *present_in_flight = false;
            }
            if matches!(surface_or_pool, Some(SurfaceOrPool::Scheme(_))) {
                *scheme_stash_clear = true;
                *scheme_skip_early_acquire = true;
            }
            apply_deferred_surface_resize(surface_or_pool, input, renderer, device_lost, present_sent_seq)
        }
        DrainResult::Idle => {
            // Scheme backend defers the present-began wait to the pre-upload barrier
            // inside `submit_to_swapchain_with` so upload recording/submit overlaps the
            // previous frame's present. Classic must still gate the acquire here.
            let is_scheme = matches!(surface_or_pool, Some(SurfaceOrPool::Scheme(_)));
            if *present_in_flight && !is_scheme {
                let _tz = goldy::tracy_zone!("velato.wait_present_ack");
                if !presenter.wait_for_present_ack() {
                    return false;
                }
                *present_in_flight = false;
            }
            true
        }
    }
}

/// Returns the stashed `PreparedFrame` when its dimensions still match the
/// current viewport, or encodes and uploads the scene from scratch.
///
/// Returns [`RenderStep::SkipFrame`] on a transient prepare error and
/// [`RenderStep::Shutdown`] on device loss.
#[cfg(feature = "use_ekrano")]
fn take_stash_or_rebuild(
    stash: Option<ekrano::PreparedFrame>,
    renderer: &mut ekrano::GoldyRenderer,
    scene: &mut ekrano::Scene,
    fragment: &mut ekrano::Scene,
    scenes: &mut SceneSet,
    input: &InputState,
    simple_text: &mut RobotoText,
    stats: &Arc<std::sync::Mutex<stats::Stats>>,
    base_color: Option<Color>,
    device_lost: &std::sync::atomic::AtomicBool,
    render_width: u32,
    render_height: u32,
) -> RenderStep<ekrano::PreparedFrame> {
    if let Some(prepared) = stash {
        if prepared.width() == render_width && prepared.height() == render_height {
            return RenderStep::Ok(prepared);
        }
    }
    let render_params = {
        let _tz = goldy::tracy_zone!("velato.build_scene_fallback");
        let stats_guard = stats.lock().expect("stats mutex poisoned");
        build_ekrano_scene(
            scene,
            fragment,
            scenes,
            input,
            simple_text,
            &stats_guard,
            base_color,
            render_width,
            render_height,
        )
    };
    match renderer.prepare(scene, &render_params) {
        Ok(prepared) => RenderStep::Ok(prepared),
        Err(e) => {
            eprintln!("Prepare error: {e}");
            if is_device_lost_error(&e) {
                device_lost.store(true, std::sync::atomic::Ordering::Relaxed);
                RenderStep::Shutdown
            } else {
                RenderStep::SkipFrame { out_of_date: false }
            }
        }
    }
}

/// Submits a [`PreparedFrame`] to the GPU surface and returns the resulting
/// [`goldy::Frame`] ready to hand off to `TID_PRESENT`.
///
/// Returns [`RenderStep::SkipFrame`] on a transient error and
/// [`RenderStep::Shutdown`] on device loss.
#[cfg(feature = "use_ekrano")]
fn try_submit_prepared(
    renderer: &mut ekrano::GoldyRenderer,
    prepared: ekrano::PreparedFrame,
    surface: &goldy::Surface,
    device_lost: &std::sync::atomic::AtomicBool,
) -> RenderStep<(ekrano::FrameStats, goldy::Frame)> {
    match renderer.submit_prepared(prepared, surface) {
        Ok(result) => RenderStep::Ok(result),
        Err(e) => {
            eprintln!("submit_prepared error: {e}");
            if is_device_lost_error(&e) {
                device_lost.store(true, std::sync::atomic::Ordering::Relaxed);
                RenderStep::Shutdown
            } else {
                RenderStep::SkipFrame {
                    out_of_date: is_swapchain_out_of_date_error(&e),
                }
            }
        }
    }
}

/// Submits a [`PreparedFrame`] to the Scheme swapchain pool and returns a
/// [`PresentToken`] for async scanout on TID_PRESENT.
///
/// Returns [`RenderStep::SkipFrame`] on a transient error and
/// [`RenderStep::Shutdown`] on device loss.
#[cfg(feature = "use_ekrano")]
fn try_submit_to_swapchain(
    renderer: &mut ekrano::GoldyRenderer,
    prepared: ekrano::PreparedFrame,
    pool: &goldy::SwapchainPool,
    device_lost: &std::sync::atomic::AtomicBool,
    _presenter: &Presenter,
    present_in_flight: &mut bool,
    present_sent_seq: u64,
) -> RenderStep<(ekrano::FrameStats, ekrano::PresentToken)> {
    use std::cell::Cell;

    // Present-began wait runs before upload submit (easement gate). Capacity wait runs
    // after upload submit so upload work overlaps the return-fence wait.
    let needs_wait = *present_in_flight;
    let consumed_present_began = Cell::new(false);
    let ctx = renderer.submission_context();
    let wait_seq = present_sent_seq;
    let using_stash = pool.has_stashed_drawable();

    let result = {
        let consumed_present_began = &consumed_present_began;
        renderer.submit_to_swapchain_with(
            prepared,
            pool,
            move || {
                if needs_wait {
                    let _tz = goldy::tracy_zone!("velato.wait_present_began");
                    pool.wait_present_began(wait_seq);
                    consumed_present_began.set(true);
                }
                Ok(())
            },
            move || {
                let _tz = goldy::tracy_zone!("velato.wait_submit_acquire");
                pool.wait_for_submit_acquire(&ctx, using_stash);
                Ok(())
            },
        )
    };

    if consumed_present_began.get() {
        *present_in_flight = false;
    }

    match result {
        Ok(r) => RenderStep::Ok(r),
        Err(e) => {
            if is_device_lost_error(&e) {
                eprintln!("submit_to_swapchain error: {e}");
                device_lost.store(true, std::sync::atomic::Ordering::Relaxed);
                RenderStep::Shutdown
            } else if is_swapchain_out_of_date_error(&e) {
                RenderStep::SkipFrame { out_of_date: true }
            } else {
                eprintln!("submit_to_swapchain error: {e}");
                RenderStep::SkipFrame { out_of_date: false }
            }
        }
    }
}

/// Build scene/params for the next frame's overlap path (CPU scene construction only).
#[cfg(feature = "use_ekrano")]
fn build_overlap_scene_params(
    scene: &mut ekrano::Scene,
    fragment: &mut ekrano::Scene,
    scenes: &mut SceneSet,
    input: &InputState,
    simple_text: &mut RobotoText,
    stats: &Arc<std::sync::Mutex<stats::Stats>>,
    base_color: Option<Color>,
    render_width: u32,
    render_height: u32,
) -> ekrano::RenderParams {
    let _tz = goldy::tracy_zone!("velato.build_scene_overlap");
    let stats_guard = stats.lock().expect("stats mutex poisoned");
    let params = build_ekrano_scene(
        scene,
        fragment,
        scenes,
        input,
        simple_text,
        &stats_guard,
        base_color,
        render_width,
        render_height,
    );
    params
}

/// Stash the next drawable during the overlap phase after dispatching a present token.
///
/// Runs on the render thread while `TID_PRESENT` consumes the previous grant, so DXGI
/// acquire latency hides behind overlap CPU work instead of sitting on the submit path.
#[cfg(feature = "use_ekrano")]
fn try_overlap_early_acquire(pool: &goldy::SwapchainPool, present_sent_seq: u64) {
    let _tz = goldy::tracy_zone!("velato.early_acquire");
    let _ = pool.try_early_acquire(present_sent_seq);
}

/// Run phase-1 `ekrano.prepare` for a scene already built by [`build_overlap_scene_params`].
#[cfg(feature = "use_ekrano")]
fn overlap_prepare_from_params(
    renderer: &mut ekrano::GoldyRenderer,
    scene: &ekrano::Scene,
    overlap_params: &ekrano::RenderParams,
) -> Option<ekrano::PreparedFrame> {
    let _tz = goldy::tracy_zone!("velato.overlap_prepare");
    renderer.prepare(scene, overlap_params).ok()
}

/// Speculatively builds the *next* frame's `PreparedFrame` while `TID_PRESENT`
/// is busy calling `swapchain.Present()` for the frame just submitted.
///
/// The result is stashed and consumed at the top of the next iteration if the
/// viewport dimensions haven't changed; otherwise it is discarded.
#[cfg(feature = "use_ekrano")]
fn build_overlap_stash(
    renderer: &mut ekrano::GoldyRenderer,
    scene: &mut ekrano::Scene,
    fragment: &mut ekrano::Scene,
    scenes: &mut SceneSet,
    input: &InputState,
    simple_text: &mut RobotoText,
    stats: &Arc<std::sync::Mutex<stats::Stats>>,
    base_color: Option<Color>,
    render_width: u32,
    render_height: u32,
) -> Option<ekrano::PreparedFrame> {
    let overlap_params = build_overlap_scene_params(
        scene,
        fragment,
        scenes,
        input,
        simple_text,
        stats,
        base_color,
        render_width,
        render_height,
    );
    overlap_prepare_from_params(renderer, scene, &overlap_params)
}

// ---------------------------------------------------------------------------
// Render thread entry point
// ---------------------------------------------------------------------------

#[cfg(feature = "use_ekrano")]
fn ekrano_render_thread(
    mut renderer: ekrano::GoldyRenderer,
    mut scenes: SceneSet,
    cmd_rx: std::sync::mpsc::Receiver<RenderCmd>,
    base_color: Option<Color>,
    stats: Arc<std::sync::Mutex<stats::Stats>>,
    device_lost: Arc<std::sync::atomic::AtomicBool>,
    initial_scene_ix: i32,
    initial_vsync: bool,
) {
    use std::sync::atomic::Ordering;
let start = Instant::now();
    let mut input = InputState {
        transform: Affine::IDENTITY,
        scene_ix: initial_scene_ix,
        complexity: 0,
        stats_shown: true,
        vsync: initial_vsync,
        width: 0,
        height: 0,
        start,
    };
    let mut prev_scene_ix = input.scene_ix - 1;
    let mut stash: Option<ekrano::PreparedFrame> = None;
    let mut scene = ekrano::Scene::new();
    let mut fragment = ekrano::Scene::new();
    let mut simple_text = RobotoText::new();
    let mut frame_start_time = Instant::now();
    let presenter = Presenter::new(Arc::clone(&device_lost));
    let mut present_in_flight = false;
    let mut present_sent_seq: u64 = 0;
    let mut surface_or_pool: Option<SurfaceOrPool> = None;

    loop {
        if device_lost.load(Ordering::Relaxed) {
break;
        }

        // Phase 1 — Sync: wait for a surface/pool, flush the previous present, then
        // consume any pending UI commands (resize, scene-switch, etc.).
        if !block_until_surface(&cmd_rx, &mut surface_or_pool, &mut input, &stats) {
return;
        }
        let mut scheme_stash_clear = false;
        let mut scheme_skip_early_acquire = false;
        if !sync_present_and_drain(
            &mut present_in_flight,
            present_sent_seq,
            &presenter,
            &cmd_rx,
            &mut surface_or_pool,
            &mut input,
            &stats,
            &mut renderer,
            &device_lost,
            &mut scheme_stash_clear,
            &mut scheme_skip_early_acquire,
        ) {
break;
        }
        if scheme_stash_clear {
            stash = None;
        }

        if surface_or_pool.is_none() {
            continue;
        }
        if input.width == 0 || input.height == 0 {
            continue;
        }

        let _frame = goldy::tracy_zone!("velato.render_frame");

        // Reset the view transform when the user switches scenes.
        let scene_ix = input.scene_ix.rem_euclid(scenes.scenes.len() as i32);
        if prev_scene_ix != scene_ix {
            input.transform = Affine::IDENTITY;
            prev_scene_ix = scene_ix;
        }

        let (render_width, render_height) =
            scheme_render_dimensions(surface_or_pool.as_ref(), &input);

        // Phase 2 — Prepare: reuse the overlap stash or rebuild the scene.
        let prepared = match take_stash_or_rebuild(
            stash.take(),
            &mut renderer,
            &mut scene,
            &mut fragment,
            &mut scenes,
            &input,
            &mut simple_text,
            &stats,
            base_color,
            &device_lost,
            render_width,
            render_height,
        ) {
            RenderStep::Ok(p) => p,
            RenderStep::SkipFrame { .. } => {
                if matches!(surface_or_pool, Some(SurfaceOrPool::Scheme(_))) {
                    stash = None;
                }
                continue;
            }
            RenderStep::Shutdown => {
break;
            }
        };

        // Phase 3 — Submit: branch on backend.
        //
        // Classic: submit_prepared acquires a swapchain image and returns a Frame token;
        //          TID_PRESENT calls frame.present() asynchronously.
        // Scheme:  submit_to_swapchain returns a PresentToken; TID_PRESENT calls
        //          grant.consume() asynchronously.
        let is_scheme = matches!(surface_or_pool, Some(SurfaceOrPool::Scheme(_)));
        if is_scheme {
            let (frame_stats, present_token): (ekrano::FrameStats, ekrano::PresentToken);
            {
                // Scope limits the immutable borrow of surface_or_pool so the SkipFrame
                // arm can reborrow it for the reactive resize.
                let pool_ref = match surface_or_pool.as_ref().unwrap() {
                    SurfaceOrPool::Scheme(p) => p,
                    _ => unreachable!(),
                };
                let submit = try_submit_to_swapchain(
                    &mut renderer,
                    prepared,
                    pool_ref,
                    &device_lost,
                    &presenter,
                    &mut present_in_flight,
                    present_sent_seq,
                );
                match submit {
                    RenderStep::Ok(r) => (frame_stats, present_token) = r,
                    RenderStep::SkipFrame { out_of_date: true } => {
                        stash = None;
                        if !reactive_scheme_resize(
                            &mut surface_or_pool,
                            &mut input,
                            &mut present_in_flight,
                            present_sent_seq,
                            &presenter,
                            &device_lost,
                            &mut renderer,
                        ) {
                            break;
                        }
                        continue;
                    }
                    RenderStep::SkipFrame { out_of_date: false } => {
                        stash = None;
                        continue;
                    }
                    RenderStep::Shutdown => {
break;
                    }
                }
            }
            if frame_stats.bump_retries > 0 {
                eprintln!(
                    "[BUMP] bump allocator reallocated {} time(s) this frame",
                    frame_stats.bump_retries,
                );
            }
            // Build next frame's scene before waking TID_PRESENT so scene CPU work
            // can overlap the previous frame's present round-trip.
            let (overlap_w, overlap_h) =
                scheme_render_dimensions(surface_or_pool.as_ref(), &input);
            let overlap_params = build_overlap_scene_params(
                &mut scene,
                &mut fragment,
                &mut scenes,
                &input,
                &mut simple_text,
                &stats,
                base_color,
                overlap_w,
                overlap_h,
            );
            if !presenter.send_present_token(present_token, &device_lost) {
break;
            }
            let prior_sent_seq = present_sent_seq;
            present_sent_seq += 1;
            present_in_flight = true;

            // Phase 4 — overlap: acquire the next drawable and prepare frame N+1 while
            // TID_PRESENT consumes the grant we just sent (present-began / WSI Present).
            // Gate on prior_sent_seq (frames already dispatched), not present_sent_seq
            // (the token we just sent — presents_begun won't have caught up yet).
            if let Some(SurfaceOrPool::Scheme(pool)) = surface_or_pool.as_ref() {
                if !scheme_skip_early_acquire {
                    try_overlap_early_acquire(pool, prior_sent_seq);
                }
            }
            stash = overlap_prepare_from_params(&mut renderer, &scene, &overlap_params);
        } else {
            // Classic path.
            let (frame_stats, frame): (ekrano::FrameStats, goldy::Frame);
            {
                let surface_ref = match surface_or_pool.as_ref().unwrap() {
                    SurfaceOrPool::Classic(s) => s,
                    _ => unreachable!(),
                };
                let submit = try_submit_prepared(&mut renderer, prepared, surface_ref, &device_lost);
                match submit {
                    RenderStep::Ok(r) => (frame_stats, frame) = r,
                    RenderStep::SkipFrame { out_of_date: true } => {
                        // Swapchain is out of date (ERROR_OUT_OF_DATE_KHR from acquire_next_image).
                        // Rebuild it reactively at the latest requested dimensions. Doing this here
                        // — rather than proactively in drain_commands — prevents the cascade of
                        // consecutive present failures caused by rebuilding before Vulkan says it
                        // is needed: that triggered device_wait_idle, during which the window moved
                        // further, making each new swapchain immediately stale.
                        // NLL ends the surface_ref borrow at the last use above; reborrow is safe.
                        if let Some(SurfaceOrPool::Classic(s)) = surface_or_pool.as_mut() {
                            let _ = s.resize(input.width, input.height);
                            // Sync input so the next frame uses the actual swapchain dimensions
                            // (may differ from requested due to Vulkan capability clamping).
                            input.width = s.width();
                            input.height = s.height();
                        }
                        continue;
                    }
                    RenderStep::SkipFrame { out_of_date: false } => continue,
                    RenderStep::Shutdown => {
break;
                    }
                }
            }
            if frame_stats.bump_retries > 0 {
                eprintln!(
                    "[BUMP] bump allocator reallocated {} time(s) this frame",
                    frame_stats.bump_retries,
                );
            }
            if !presenter.send_frame(frame, &device_lost) {
break;
            }
            present_in_flight = true;

            // Phase 4 — Overlap: build the next frame's scene on the CPU while
            // TID_PRESENT calls swapchain.Present() for the frame just submitted.
            let (overlap_w, overlap_h) =
                scheme_render_dimensions(surface_or_pool.as_ref(), &input);
            stash = build_overlap_stash(
                &mut renderer,
                &mut scene,
                &mut fragment,
                &mut scenes,
                &input,
                &mut simple_text,
                &stats,
                base_color,
                overlap_w,
                overlap_h,
            );
        }

        let new_time = Instant::now();
        let _tz = goldy::tracy_zone!("velato.record_stats");
        stats
            .lock()
            .expect("stats mutex poisoned")
            .add_sample(stats::Sample {
                frame_time_us: (new_time - frame_start_time).as_micros() as u64,
            });
        frame_start_time = new_time;
    }
presenter.shutdown();
}

#[cfg(feature = "use_ekrano")]
fn run_ekrano(event_loop: EventLoop<()>, args: Args, scenes: SceneSet) {
    use ekrano::GoldyRenderer;
    use goldy::{
        DeviceDescriptor, Instance, PresentMode, RequestAdapterOptions, Surface, SurfaceConfig,
        SwapchainPool,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex};
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
        .request_adapter(&RequestAdapterOptions::default())
        .expect("No GPU adapter found")
        .request_device(&DeviceDescriptor::default())
        .expect("No GPU device found");
    let start_create = Instant::now();
    let renderer = GoldyRenderer::new(&device).expect("Failed to create ekrano renderer");
    // Clone the renderer's submission context so the surface can be created on
    // the same timeline semaphore as the renderer. This ensures gpu_progress()
    // and BoundaryCrossed signals fired by the renderer's poller correctly
    // reflect surface-frame completion — the fix for the RT-cache/reclamation
    // mismatch introduced by goldy #179 increment 3a per-context timelines.
    let render_ctx = renderer.submission_context();
    let backend = renderer.backend();
    eprintln!("Creating ekrano renderer took {:?}", start_create.elapsed());

    let vsync = !args.no_vsync;
    let initial_scene_ix = args.scene.unwrap_or(0);
    let base_color = args.args.base_color;
    let scene_names: Vec<String> = scenes
        .scenes
        .iter()
        .map(|scene| scene.config.name.clone())
        .collect();
    let auto_exit_deadline = args
        .timeout_secs
        .map(|s| Instant::now() + std::time::Duration::from_secs(s));

    let stats = Arc::new(Mutex::new(stats::Stats::new()));
    let device_lost = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = mpsc::channel::<RenderCmd>();

    let render_stats = Arc::clone(&stats);
    let render_device_lost = Arc::clone(&device_lost);
    let bench_stats = Arc::clone(&stats);
    let render_thread = std::thread::spawn(move || {
        ekrano_render_thread(
            renderer,
            scenes,
            cmd_rx,
            base_color,
            render_stats,
            render_device_lost,
            initial_scene_ix,
            vsync,
        );
    });

    let mut window: Option<Arc<Window>> = None;
    let mut cmd_tx: Option<Sender<RenderCmd>> = Some(cmd_tx);
    let mut touch_state = multi_touch::TouchState::new();
    let mut transform = Affine::IDENTITY;
    let mut mouse_down = false;
    let mut prior_position: Option<Vec2> = None;
    let mut scene_ix = initial_scene_ix;
    let mut prev_scene_ix = scene_ix - 1;

    let send_cmd = |tx: &Option<Sender<RenderCmd>>, cmd: RenderCmd| {
        if let Some(tx) = tx {
            if let Err(err) = tx.send(cmd) {
                tracing::warn!("failed to send render command: {err}");
            }
        }
    };

    event_loop
        .run(move |event, event_loop| match event {
            Event::WindowEvent {
                ref event,
                window_id,
            } => {
                let _tz = goldy::tracy_zone!("velato.ui_window_event");
                let Some(win) = &window else { return };
                if win.id() != window_id {
                    return;
                }
                match event {
                    WindowEvent::CloseRequested => {
send_cmd(&cmd_tx, RenderCmd::Shutdown);
                        event_loop.exit();
                    }
                    WindowEvent::ModifiersChanged(_) => {}
                    WindowEvent::KeyboardInput { event, .. } => {
                        if event.state == ElementState::Pressed {
                            match event.logical_key.as_ref() {
                                Key::Named(NamedKey::ArrowLeft) => {
                                    scene_ix = scene_ix.saturating_sub(1);
                                    send_cmd(&cmd_tx, RenderCmd::SceneDelta(-1));
                                }
                                Key::Named(NamedKey::ArrowRight) => {
                                    scene_ix = scene_ix.saturating_add(1);
                                    send_cmd(&cmd_tx, RenderCmd::SceneDelta(1));
                                }
                                Key::Named(NamedKey::ArrowUp) => {
                                    send_cmd(&cmd_tx, RenderCmd::ComplexityDelta(1));
                                }
                                Key::Named(NamedKey::ArrowDown) => {
                                    send_cmd(&cmd_tx, RenderCmd::ComplexityDelta(-1));
                                }
                                Key::Named(NamedKey::Space) => {
                                    transform = Affine::IDENTITY;
                                    send_cmd(&cmd_tx, RenderCmd::TransformSet(transform));
                                }
                                Key::Named(NamedKey::Escape) => {
send_cmd(&cmd_tx, RenderCmd::Shutdown);
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
                                                send_cmd(&cmd_tx, RenderCmd::TransformSet(transform));
                                            }
                                        }
                                        "s" => send_cmd(&cmd_tx, RenderCmd::ToggleStats),
                                        "c" => {
                                            send_cmd(&cmd_tx, RenderCmd::ClearStats);
                                            if let Ok(mut stats) = stats.lock() {
                                                stats.clear_min_and_max();
                                            }
                                        }
                                        "d" => {
                                            eprintln!(
                                                "Complexity overlay toggling not available in ekrano mode"
                                            );
                                        }
                                        "m" => {
                                            eprintln!(
                                                "AA method switching not available in ekrano mode"
                                            );
                                        }
                                        "v" => {
                                            if !event.repeat {
                                                send_cmd(&cmd_tx, RenderCmd::ToggleVsync);
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
                        send_cmd(
                            &cmd_tx,
                            RenderCmd::Resize(size.width.max(1), size.height.max(1)),
                        );
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
                            send_cmd(&cmd_tx, RenderCmd::TransformSet(transform));
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
                            send_cmd(&cmd_tx, RenderCmd::TransformSet(transform));
                        }
                        prior_position = Some(position);
                    }
                    WindowEvent::RedrawRequested => {}
                    _ => {}
                }
            }
            Event::AboutToWait => {
                let _tz = goldy::tracy_zone!("velato.ui_about_to_wait");
                touch_state.end_frame();
                if let Some(touch_info) = touch_state.info() {
                    let centre = Vec2::new(touch_info.zoom_centre.x, touch_info.zoom_centre.y);
                    transform = Affine::translate(touch_info.translation_delta)
                        * Affine::translate(centre)
                        * Affine::scale(touch_info.zoom_delta)
                        * Affine::rotate(touch_info.rotation_delta)
                        * Affine::translate(-centre)
                        * transform;
                    send_cmd(&cmd_tx, RenderCmd::TransformSet(transform));
                }
                if device_lost.load(Ordering::Relaxed) {
                    eprintln!("GPU device lost — exiting");
send_cmd(&cmd_tx, RenderCmd::Shutdown);
                    event_loop.exit();
                    return;
                }
                if let Some(deadline) = auto_exit_deadline {
                    if Instant::now() >= deadline {
send_cmd(&cmd_tx, RenderCmd::Shutdown);
                        event_loop.exit();
                        return;
                    }
                }
                if let Some(win) = &window {
                    let scene_count = scene_names.len().max(1) as i32;
                    scene_ix = scene_ix.rem_euclid(scene_count);
                    if prev_scene_ix != scene_ix {
                        prev_scene_ix = scene_ix;
                        transform = Affine::IDENTITY;
                        send_cmd(&cmd_tx, RenderCmd::TransformSet(transform));
                        let title = scene_names
                            .get(scene_ix as usize)
                            .map(|name| format!("Ekrano demo - {name}"))
                            .unwrap_or_else(|| "Ekrano demo".to_string());
                        win.set_title(&title);
                    }
                }
            }
            Event::Resumed => {
                let _tz = goldy::tracy_zone!("velato.ui_resumed");
                let win = create_ekrano_window(event_loop);
                let initial_mode = if vsync {
                    PresentMode::Fifo
                } else {
                    PresentMode::Immediate
                };
                let config = SurfaceConfig {
                    present_mode: initial_mode,
                    depth_format: None,
                };
                let sop = match backend {
                    ekrano::GoldyBackend::Classic => {
                        let surf = Surface::new_with_config(&render_ctx, win.as_ref(), config)
                            .expect("Failed to create goldy surface");
                        SurfaceOrPool::Classic(surf)
                    }
                    ekrano::GoldyBackend::Scheme => {
                        let pool = SwapchainPool::new_with_options(
                            &render_ctx,
                            win.as_ref(),
                            scheme_pool_options(device.backend_type(), config),
                        )
                        .expect("Failed to create goldy swapchain pool");
                        SurfaceOrPool::Scheme(pool)
                    }
                };
                send_cmd(&cmd_tx, RenderCmd::SurfaceCreated(sop));
                window = Some(win);
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Event::Suspended => {
                let _tz = goldy::tracy_zone!("velato.ui_suspended");
                send_cmd(&cmd_tx, RenderCmd::SurfaceDropped);
                window = None;
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Event::LoopExiting => {
                let _tz = goldy::tracy_zone!("velato.ui_loop_exiting");
cmd_tx.take();
                window = None;
            }
            _ => {}
        })
        .expect("run to completion");
let join_start = Instant::now();
let _ = render_thread.join();
let snap = bench_stats.lock().expect("stats mutex poisoned").snapshot();
    eprintln!(
        "[bench] fps={:.1} frame_ms={:.3} min_ms={:.3} max_ms={:.3}",
        snap.fps, snap.frame_time_ms, snap.frame_time_min_ms, snap.frame_time_max_ms
    );
}

/// # Panics
/// Can panic.
#[cfg(feature = "use_ekrano")]
pub fn main() -> Result<()> {
    eprintln!("=== EKRANO BACKEND ACTIVE (pid {}) ===", std::process::id());
    // Goldy logs its GPU diagnostics (timeouts, completion-handler errors,
    // descriptor encode paths) via `tracing`. `env_logger` only understands
    // the `log` crate, so those events never reach the terminal. Installing
    // a tracing-subscriber fmt layer that honors `RUST_LOG` surfaces them.
    // Default to `warn` so a plain `cargo run` produces clean stdout suitable
    // for FPS comparisons against upstream vello. Goldy / ekrano startup
    // tracing and per-frame perf heartbeats are still reachable via
    // `RUST_LOG=goldy=info,ekrano=debug` or similar.
    // Scheme retention diagnosis: `RUST_LOG=goldy::scheme=debug`.
    // Worker retention diagnosis: `RUST_LOG=ekrano=debug` (look for `[WORKER-STALE]`).
    // Shutdown stall diagnosis: `RUST_LOG=velato::shutdown=info`.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    // Route `log::*` records into the tracing subscriber so legacy callers
    // (velato itself, scenes, etc.) interleave with goldy's tracing output
    // rather than disappearing.
    let _ = tracing_log::LogTracer::init();
    dump_backend_info("ekrano");
    let args = Args::parse();
    let scenes = args.args.select_scene_set(Args::command)?;
    if let Some(scenes) = scenes {
        let event_loop = EventLoopBuilder::<()>::with_user_event().build()?;
        run_ekrano(event_loop, args, scenes);
}
    Ok(())
}
