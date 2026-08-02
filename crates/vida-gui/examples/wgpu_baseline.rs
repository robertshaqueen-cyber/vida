#![allow(unexpected_cfgs)]

use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    width: u32,
    height: u32,
    force_scale: f64,
}

struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

impl App {
    fn new(width: u32, height: u32, force_scale: f64) -> Self {
        Self {
            window: None,
            renderer: None,
            width,
            height,
            force_scale,
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("winit+wgpu baseline")
            .with_inner_size(winit::dpi::LogicalSize::new(self.width as f64, self.height as f64));

        let window = Arc::new(event_loop.create_window(attrs).unwrap());

        // Force scale factor on macOS if requested
        #[cfg(target_os = "macos")]
        if self.force_scale > 1.0 {
            use raw_window_handle::HasWindowHandle;
            if let Ok(wh) = window.window_handle() {
                use raw_window_handle::RawWindowHandle;
                if let RawWindowHandle::AppKit(awh) = wh.as_raw() {
                    unsafe {
                        use objc::msg_send;
                        use objc::sel;
                        use objc::sel_impl;
                        use objc::runtime::Object;
                        let ns_view = awh.ns_view.as_ptr() as *mut Object;
                        let ns_window: *mut Object = msg_send![ns_view, window];
                        if !ns_window.is_null() {
                            let _: () = msg_send![ns_window,
                                setBackingScaleFactor: self.force_scale];
                        }
                    }
                }
            }
        }

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..wgpu::InstanceDescriptor::from_env_or_default()
        });

        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .unwrap();

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("baseline"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .unwrap();

        let size = window.inner_size();
        let scale = window.scale_factor();
        eprintln!("Window: {}x{} logical, {}x{} physical, scale={}",
            self.width, self.height, size.width, size.height, scale);

        let cap = surface.get_capabilities(&adapter);
        let format = cap.formats[0];
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: cap.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        self.window = Some(window);
        self.renderer = Some(Renderer {
            device,
            queue,
            surface,
            config,
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if let Some(r) = &mut self.renderer {
                    r.config.width = new_size.width.max(1);
                    r.config.height = new_size.height.max(1);
                    r.surface.configure(&r.device, &r.config);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = &self.renderer {
                    let output = r.surface.get_current_texture().unwrap();
                    let view = output
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    let mut encoder = r
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("baseline encoder"),
                        });
                    {
                        let _rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("baseline pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color {
                                        r: 0.1,
                                        g: 0.1,
                                        b: 0.15,
                                        a: 1.0,
                                    }),
                                    store: wgpu::StoreOp::Store,
                                },
                                depth_slice: None,
                            })],
                            depth_stencil_attachment: None,
                            ..Default::default()
                        });
                    }
                    r.queue.submit(std::iter::once(encoder.finish()));
                    output.present();
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let width = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(800);
    let height = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(600);
    let scale: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(width, height, scale);
    event_loop.run_app(&mut app).unwrap();
}
