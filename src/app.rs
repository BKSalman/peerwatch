use std::num::NonZeroU32;
use std::sync::Arc;

use glutin::context::PossiblyCurrentContext;
use glutin::prelude::*;
use glutin::surface::{Surface, WindowSurface};
use libmpv2::Mpv;
use libmpv2::render::RenderContext;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

pub struct App {
    render_context: RenderContext,
    gl_surface: Surface<WindowSurface>,
    gl_context: PossiblyCurrentContext,
    mpv: Arc<Mpv>,
    window: Option<Window>,
}

impl App {
    pub fn new(
        render_context: RenderContext,
        gl_surface: Surface<WindowSurface>,
        gl_context: PossiblyCurrentContext,
        mpv: Arc<Mpv>,
    ) -> Self {
        Self {
            window: None,
            render_context,
            gl_surface,
            gl_context,
            mpv,
        }
    }
}

impl ApplicationHandler<crate::Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.window = Some(
            event_loop
                .create_window(Window::default_attributes())
                .unwrap(),
        );
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: crate::Event) {
        match event {
            crate::Event::MpvEvent(event) => match event {
                _ => {}
            },
            crate::Event::PeerEvent(_peer_event) => todo!(),
            crate::Event::RedrawRequested => {
                let window = self.window.as_ref().unwrap();
                window.request_redraw();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => {
                let window = self.window.as_ref().unwrap();

                let size = window.inner_size();

                self.render_context
                    .render::<glutin::display::Display>(0, size.width as _, size.height as _, true)
                    .expect("Failed to draw on window");

                // TODO: not sure if this is necessary, was taken from
                // https://github.com/rust-windowing/winit/blob/f6893a4390dfe6118ce4b33458d458fd3efd3025/examples/window.rs#L864
                // but it stops `WindowEvent::RedrawRequested` from being envoked
                // window.pre_present_notify();

                window.request_redraw();

                self.gl_surface.swap_buffers(&self.gl_context).unwrap();
                self.render_context.report_swap();
            }
            WindowEvent::Resized(size) if size.width != 0 && size.height != 0 => {
                let window = self.window.as_ref().unwrap();
                self.gl_surface.resize(
                    &self.gl_context,
                    NonZeroU32::new(size.width).unwrap(),
                    NonZeroU32::new(size.height).unwrap(),
                );
                window.request_redraw();
            }
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            _ => (),
        }
    }
}
