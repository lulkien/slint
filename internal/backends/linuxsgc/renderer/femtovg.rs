// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use std::{cell::Cell, cell::RefCell, num::NonZeroU32, rc::Rc};

use i_slint_core::item_rendering::ItemRenderer;
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::DrawOutcome;
use i_slint_renderer_femtovg::{FemtoVGOpenGLRendererExt, FemtoVGRendererExt};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use glutin::{
    context::{ContextApi, ContextAttributesBuilder},
    display::GetGlDisplay,
    prelude::*,
    surface::{SurfaceAttributesBuilder, WindowSurface},
};

use crate::display::{Presenter, RenderingRotation, gbmdisplay::GbmDisplay};
use crate::drmoutput::DrmOutput;

pub struct FemtoVGRendererAdapter {
    renderer:
        i_slint_renderer_femtovg::FemtoVGRenderer<i_slint_renderer_femtovg::opengl::OpenGLBackend>,
    // sgc fork: both are replaced when a re-granted lease moves the display stack
    // to a new fd. The renderer object itself stays where it is (the OpenGL
    // interface and the canvas live inside it), because the core keeps a
    // `&dyn Renderer` to it for the lifetime of the window.
    gbm_display: RefCell<Rc<GbmDisplay>>,
    size: Cell<i_slint_core::api::PhysicalSize>,
}

struct GlContextWrapper {
    glutin_context: glutin::context::PossiblyCurrentContext,
    glutin_surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    gbm_display: Rc<GbmDisplay>,
}

impl GlContextWrapper {
    fn new(gbm_display: &Rc<GbmDisplay>) -> Result<Self, PlatformError> {
        let (width, height) = gbm_display.drm_output.size();
        let width: std::num::NonZeroU32 = width.try_into().map_err(|_| {
            format!("Attempting to create window surface with an invalid width: {}", width)
        })?;
        let height: std::num::NonZeroU32 = height.try_into().map_err(|_| {
            format!("Attempting to create window surface with an invalid height: {}", height)
        })?;

        let display_handle = gbm_display.display_handle().unwrap();
        let window_handle = gbm_display.window_handle().unwrap();

        let gl_display = unsafe {
            glutin::display::Display::new(
                display_handle.as_raw(),
                glutin::display::DisplayApiPreference::Egl,
            )
            .map_err(|e| format!("Error creating EGL display: {e}"))?
        };

        let config_template = gbm_display.config_template_builder().build();

        let config = unsafe {
            gl_display
                .find_configs(config_template)
                .map_err(|e| format!("Error locating EGL configs: {e}"))?
                .filter(|config| gbm_display.filter_gl_config(config))
                .reduce(|accum, config| {
                    let transparency_check = config.supports_transparency().unwrap_or(false)
                        & !accum.supports_transparency().unwrap_or(false);

                    if transparency_check || config.num_samples() < accum.num_samples() {
                        config
                    } else {
                        accum
                    }
                })
                .ok_or("Unable to find suitable GL config")?
        };

        let gles_context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(glutin::context::Version {
                major: 2,
                minor: 0,
            })))
            .build(Some(window_handle.as_raw()));

        let fallback_context_attributes =
            ContextAttributesBuilder::new().build(Some(window_handle.as_raw()));

        let not_current_gl_context = unsafe {
            gl_display
                .create_context(&config, &gles_context_attributes)
                .or_else(|_| gl_display.create_context(&config, &fallback_context_attributes))
                .map_err(|e| format!("Error creating EGL context: {e}"))?
        };

        let attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            window_handle.as_raw(),
            width,
            height,
        );

        let surface = unsafe {
            config
                .display()
                .create_window_surface(&config, &attrs)
                .map_err(|e| format!("Error creating EGL window surface: {e}"))?
        };

        let context = not_current_gl_context.make_current(&surface)
        .map_err(|glutin_error: glutin::error::Error| -> PlatformError {
            format!("FemtoVG Renderer: Failed to make newly created OpenGL context current: {glutin_error}")
            .into()
    })?;

        Ok(Self {
            glutin_context: context,
            glutin_surface: surface,
            gbm_display: gbm_display.clone(),
        })
    }
}

unsafe impl i_slint_renderer_femtovg::opengl::OpenGLInterface for GlContextWrapper {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.glutin_context.is_current() {
            self.glutin_context.make_current(&self.glutin_surface).map_err(
                |glutin_error| -> PlatformError {
                    format!("FemtoVG: Error making context current: {glutin_error}").into()
                },
            )?;
        }
        Ok(())
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Make sure the in-flight font-buffer from the previous swap_buffers call has been
        // posted to the screen.
        self.gbm_display.drm_output.wait_for_page_flip();
        self.glutin_surface.swap_buffers(&self.glutin_context).map_err(
            |glutin_error| -> PlatformError {
                format!("FemtoVG: Error swapping buffers: {glutin_error}").into()
            },
        )?;
        Ok(())
    }

    fn resize(
        &self,
        _width: NonZeroU32,
        _height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Ignore resize requests
        Ok(())
    }

    fn get_proc_address(&self, name: &std::ffi::CStr) -> *const std::ffi::c_void {
        self.glutin_context.display().get_proc_address(name)
    }
}

impl FemtoVGRendererAdapter {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        device_opener: &crate::DeviceOpener,
        _requested_graphics_api: Option<&i_slint_core::graphics::RequestedGraphicsAPI>,
    ) -> Result<Box<dyn crate::fullscreenwindowadapter::FullscreenRenderer>, PlatformError> {
        let drm_output = DrmOutput::new(device_opener)?;
        let egl_display = Rc::new(crate::display::gbmdisplay::GbmDisplay::new(drm_output)?);

        let (width, height) = egl_display.drm_output.size();
        let size = i_slint_core::api::PhysicalSize::new(width, height);

        let renderer = Box::new(Self {
            renderer: i_slint_renderer_femtovg::FemtoVGRenderer::new(GlContextWrapper::new(
                &egl_display,
            )?)?,
            gbm_display: RefCell::new(egl_display),
            size: Cell::new(size),
        });

        eprintln!("Using FemtoVG OpenGL renderer");

        Ok(renderer)
    }
}

impl crate::fullscreenwindowadapter::FullscreenRenderer for FemtoVGRendererAdapter {
    fn as_core_renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &self.renderer
    }

    fn render_and_present(
        &self,
        rotation: RenderingRotation,
        _mouse_position: Option<i_slint_core::api::PhysicalPosition>,
        draw_mouse_cursor_callback: &dyn Fn(&mut dyn ItemRenderer),
    ) -> Result<DrawOutcome, PlatformError> {
        let size = self.size.get();
        let outcome = self.renderer.render_transformed_with_post_callback(
            rotation.degrees(),
            rotation.translation_after_rotation(size),
            size,
            Some(&|item_renderer| {
                draw_mouse_cursor_callback(item_renderer);
            }),
        )?;
        if matches!(outcome, DrawOutcome::Success) {
            self.gbm_display.borrow().present()?;
        }
        Ok(outcome)
    }
    fn size(&self) -> i_slint_core::api::PhysicalSize {
        self.size.get()
    }

    /// sgc fork: rebuild the display stack on the lease the daemon just granted.
    ///
    /// The EGL context, its surface and the GBM device were all created from the
    /// revoked lease fd, and EGL cannot re-home a live context onto another
    /// device. So the femtovg canvas and the GL caches are dropped first - while
    /// the old context is still current, which is what `clear_graphics_context`
    /// needs - and a fresh stack is then built from the re-granted lease and
    /// handed to the same renderer object.
    ///
    /// The next frame creates a new femtovg canvas, so it re-uploads the glyph
    /// atlas and every texture. That frame has to cover the whole window, which is
    /// what `rebuild_renderer` asks for after this returns.
    fn rebuild(&self, device_opener: &crate::DeviceOpener) -> Result<(), PlatformError> {
        // Best effort: a revoked device may refuse to make its context current
        // again. Leaking the old canvas is better than refusing to resume, and the
        // caches below are dropped regardless.
        if let Err(err) = self.renderer.clear_graphics_context() {
            eprintln!("linuxsgc: tearing down the revoked GL context failed: {err}");
        }

        let drm_output = DrmOutput::new(device_opener)?;
        let gbm_display = Rc::new(crate::display::gbmdisplay::GbmDisplay::new(drm_output)?);
        let (width, height) = gbm_display.drm_output.size();

        self.renderer.set_opengl_context(GlContextWrapper::new(&gbm_display)?)?;

        // Only now is it safe to drop the old stack: the new context owns the new
        // GBM device, and the revoked one is released with the old Rc.
        *self.gbm_display.borrow_mut() = gbm_display;
        self.size.set(i_slint_core::api::PhysicalSize::new(width, height));

        println!("linuxsgc: display stack rebuilt on the re-granted lease ({width}x{height})");
        Ok(())
    }
}
