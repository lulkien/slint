// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Module containing the software renderer

use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::DrawOutcome;
pub use i_slint_renderer_software::SoftwareRenderer;
use i_slint_renderer_software::{PremultipliedRgbaColor, RepaintBufferType, TargetPixel};
use std::rc::Rc;

use crate::display::RenderingRotation;

pub struct SoftwareRendererAdapter {
    renderer: SoftwareRenderer,
    /// fd-bound display stack; rebuilt on lease re-grant (the SoftwareRenderer
    /// itself is fd-independent and survives a rebuild).
    display: std::cell::RefCell<Option<Rc<dyn crate::display::swdisplay::SoftwareBufferDisplay>>>,
    presenter: std::cell::RefCell<Option<Rc<dyn crate::display::Presenter>>>,
    size: std::cell::Cell<i_slint_core::api::PhysicalSize>,
}

const SOFTWARE_RENDER_SUPPORTED_DRM_FOURCC_FORMATS: &[drm::buffer::DrmFourcc] = &[
    // Preferred formats
    drm::buffer::DrmFourcc::Xrgb8888,
    drm::buffer::DrmFourcc::Argb8888,
    drm::buffer::DrmFourcc::Bgra8888,
    // drm::buffer::DrmFourcc::Rgba8888,

    // 16-bit formats
    drm::buffer::DrmFourcc::Rgb565,
    // drm::buffer::DrmFourcc::Bgr565,

    // // 4444 formats
    // drm::buffer::DrmFourcc::Argb4444,
    // drm::buffer::DrmFourcc::Abgr4444,
    // drm::buffer::DrmFourcc::Rgba4444,
    // drm::buffer::DrmFourcc::Bgra4444,

    // // Single channel formats
    // drm::buffer::DrmFourcc::Gray8,
    // drm::buffer::DrmFourcc::C8,
    // drm::buffer::DrmFourcc::R8,
    // drm::buffer::DrmFourcc::R16,

    // // Dual channel formats
    // drm::buffer::DrmFourcc::Gr88,
    // drm::buffer::DrmFourcc::Rg88,
    // drm::buffer::DrmFourcc::Gr1616,
    // drm::buffer::DrmFourcc::Rg1616,

    // // 10-bit formats
    // drm::buffer::DrmFourcc::Xrgb2101010,
    // drm::buffer::DrmFourcc::Argb2101010,
    // drm::buffer::DrmFourcc::Abgr2101010,
    // drm::buffer::DrmFourcc::Rgba1010102,
    // drm::buffer::DrmFourcc::Bgra1010102,
    // drm::buffer::DrmFourcc::Rgbx1010102,
    // drm::buffer::DrmFourcc::Bgrx1010102,
];

#[repr(transparent)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct DumbBufferPixelXrgb888(pub u32);

#[repr(transparent)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct DumbBufferPixelBgra8888(pub u32);

impl From<DumbBufferPixelXrgb888> for PremultipliedRgbaColor {
    #[inline]
    fn from(pixel: DumbBufferPixelXrgb888) -> Self {
        let v = pixel.0;
        PremultipliedRgbaColor {
            red: (v >> 16) as u8,
            green: (v >> 8) as u8,
            blue: v as u8,
            alpha: (v >> 24) as u8,
        }
    }
}

impl From<PremultipliedRgbaColor> for DumbBufferPixelXrgb888 {
    #[inline]
    fn from(pixel: PremultipliedRgbaColor) -> Self {
        Self(
            (pixel.alpha as u32) << 24
                | ((pixel.red as u32) << 16)
                | ((pixel.green as u32) << 8)
                | (pixel.blue as u32),
        )
    }
}

impl From<DumbBufferPixelBgra8888> for PremultipliedRgbaColor {
    #[inline]
    fn from(pixel: DumbBufferPixelBgra8888) -> Self {
        let v = pixel.0;
        PremultipliedRgbaColor {
            red: (v >> 8) as u8,
            green: (v >> 16) as u8,
            blue: (v >> 24) as u8,
            alpha: v as u8,
        }
    }
}

impl From<PremultipliedRgbaColor> for DumbBufferPixelBgra8888 {
    #[inline]
    fn from(pixel: PremultipliedRgbaColor) -> Self {
        Self(
            pixel.alpha as u32
                | ((pixel.red as u32) << 8)
                | ((pixel.green as u32) << 16)
                | ((pixel.blue as u32) << 24),
        )
    }
}

impl TargetPixel for DumbBufferPixelXrgb888 {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let mut x = PremultipliedRgbaColor::from(*self);
        x.blend(color);
        *self = x.into();
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(0xff000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    fn background() -> Self {
        Self(0)
    }
}

impl TargetPixel for DumbBufferPixelBgra8888 {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let mut x = PremultipliedRgbaColor::from(*self);
        x.blend(color);
        *self = x.into();
    }
    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(0x000000ff | ((r as u32) << 8) | ((g as u32) << 16) | ((b as u32) << 24))
    }
    fn background() -> Self {
        Self(0)
    }
}

impl SoftwareRendererAdapter {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        device_opener: &crate::DeviceOpener,
        _requested_graphics_api: Option<&i_slint_core::graphics::RequestedGraphicsAPI>,
    ) -> Result<Box<dyn crate::fullscreenwindowadapter::FullscreenRenderer>, PlatformError> {
        let renderer = Box::new(Self {
            renderer: SoftwareRenderer::new(),
            display: Default::default(),
            presenter: Default::default(),
            size: Default::default(),
        });

        renderer.init_display(device_opener)?;

        eprintln!("Using Software renderer");

        Ok(renderer)
    }

    /// (Re)create the fd-bound display stack (dumb buffers, framebuffers, crtc
    /// state) on the current device. Called at startup and again when a DRM
    /// lease is re-granted after a revoke: the old stack died with the revoked
    /// lease fd, everything display-side is rebuilt on the fresh fd. The
    /// SoftwareRenderer itself is untouched (fd-independent); the next render
    /// paints the first frame of the new stack fully (fresh buffer age).
    fn init_display(&self, device_opener: &crate::DeviceOpener) -> Result<(), PlatformError> {
        let display = crate::display::swdisplay::new(
            device_opener,
            SOFTWARE_RENDER_SUPPORTED_DRM_FOURCC_FORMATS,
        )?;

        let (width, height) = display.size();
        let size = i_slint_core::api::PhysicalSize::new(width, height);

        *self.display.borrow_mut() = Some(display.clone());
        *self.presenter.borrow_mut() = Some(display.as_presenter());
        self.size.set(size);
        Ok(())
    }

    fn current_display(
        &self,
    ) -> std::cell::Ref<'_, Rc<dyn crate::display::swdisplay::SoftwareBufferDisplay>> {
        std::cell::Ref::map(self.display.borrow(), |d| d.as_ref().expect("display initialized"))
    }
}

impl crate::fullscreenwindowadapter::FullscreenRenderer for SoftwareRendererAdapter {
    fn as_core_renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &self.renderer
    }

    fn render_and_present(
        &self,
        rotation: RenderingRotation,
        mouse_position: Option<i_slint_core::api::PhysicalPosition>,
        _draw_mouse_cursor_callback: &dyn Fn(&mut dyn i_slint_core::item_rendering::ItemRenderer),
    ) -> Result<DrawOutcome, PlatformError> {
        let size = self.size.get();
        // The (cached) cursor bitmap to composite over the frame, when a
        // pointer exists: the rasterized cursor pixels (an Rc clone — cheap).
        // Extracted before mapping the back buffer so the pixel slice can be
        // borrowed inside the render closure.
        let cursor = mouse_position.map(|_| {
            let image = crate::fullscreenwindowadapter::mouse_cursor_image();
            let inner: &i_slint_core::graphics::ImageInner = (&image).into();
            let i_slint_core::graphics::ImageInner::EmbeddedImage { buffer, .. } = inner else {
                unreachable!("mouse_cursor_image always returns the rasterized cursor");
            };
            match buffer {
                // svg.render produces RGBA8Premultiplied; both variants carry
                // the same pixel type, and blending treats the source as
                // premultiplied either way.
                i_slint_core::graphics::SharedImageBuffer::RGBA8(pixels)
                | i_slint_core::graphics::SharedImageBuffer::RGBA8Premultiplied(pixels) => {
                    pixels.clone()
                }
                i_slint_core::graphics::SharedImageBuffer::RGB8(_) => {
                    unreachable!("the cursor image is RGBA")
                }
            }
        });
        self.current_display().map_back_buffer(&mut |pixels, age, format| {
            self.renderer.set_repaint_buffer_type(match age {
                1 => RepaintBufferType::ReusedBuffer,
                2 => RepaintBufferType::SwappedBuffers,
                _ => RepaintBufferType::NewBuffer,
            });

            self.renderer.set_rendering_rotation(match rotation {
                RenderingRotation::NoRotation => {
                    i_slint_renderer_software::RenderingRotation::NoRotation
                }
                RenderingRotation::Rotate90 => {
                    i_slint_renderer_software::RenderingRotation::Rotate90
                }
                RenderingRotation::Rotate180 => {
                    i_slint_renderer_software::RenderingRotation::Rotate180
                }
                RenderingRotation::Rotate270 => {
                    i_slint_renderer_software::RenderingRotation::Rotate270
                }
            });

            let cursor = cursor.as_ref().map(|pixels| {
                (pixels.width() as usize, pixels.height() as usize, pixels.as_slice())
            });

            match format {
                drm::buffer::DrmFourcc::Xrgb8888 | drm::buffer::DrmFourcc::Argb8888 => {
                    let buffer: &mut [DumbBufferPixelXrgb888] = bytemuck::cast_slice_mut(pixels);
                    self.renderer.render(buffer, size.width as usize);
                    if let (
                        Some((cursor_width, cursor_height, cursor_pixels)),
                        Some(mouse_position),
                    ) = (cursor, mouse_position)
                    {
                        composite_cursor(
                            buffer,
                            size.width as usize,
                            size.height as usize,
                            rotation,
                            mouse_position,
                            cursor_pixels,
                            cursor_width,
                            cursor_height,
                        );
                    }
                }

                drm::buffer::DrmFourcc::Bgra8888 => {
                    let buffer: &mut [DumbBufferPixelBgra8888] = bytemuck::cast_slice_mut(pixels);
                    self.renderer.render(buffer, size.width as usize);
                    if let (
                        Some((cursor_width, cursor_height, cursor_pixels)),
                        Some(mouse_position),
                    ) = (cursor, mouse_position)
                    {
                        composite_cursor(
                            buffer,
                            size.width as usize,
                            size.height as usize,
                            rotation,
                            mouse_position,
                            cursor_pixels,
                            cursor_width,
                            cursor_height,
                        );
                    }
                }
                drm::buffer::DrmFourcc::Rgb565 => {
                    let buffer: &mut [i_slint_renderer_software::Rgb565Pixel] =
                        bytemuck::cast_slice_mut(pixels);
                    self.renderer.render(buffer, size.width as usize);
                    if let (
                        Some((cursor_width, cursor_height, cursor_pixels)),
                        Some(mouse_position),
                    ) = (cursor, mouse_position)
                    {
                        composite_cursor(
                            buffer,
                            size.width as usize,
                            size.height as usize,
                            rotation,
                            mouse_position,
                            cursor_pixels,
                            cursor_width,
                            cursor_height,
                        );
                    }
                }
                _ => {
                    return Err(format!(
                        "Unsupported frame buffer format {format} used with software renderer"
                    )
                    .into());
                }
            }

            Ok(())
        })?;
        self.presenter.borrow().as_ref().expect("presenter initialized").present()?;
        Ok(DrawOutcome::Success)
    }

    fn size(&self) -> i_slint_core::api::PhysicalSize {
        self.size.get()
    }

    fn rebuild(&self, device_opener: &crate::DeviceOpener) -> Result<(), PlatformError> {
        self.init_display(device_opener)?;
        Ok(())
    }
}

/// Blend the cached mouse-cursor bitmap over the rendered frame at the
/// pointer position (window physical coordinates, post-scale). The buffer
/// holds the frame in the screen's orientation, so window coordinates are
/// mapped with the same mirror + transpose transform the software renderer
/// applies to the scene — the cursor tracks the pointer under any
/// SLINT_KMS_ROTATION. The cursor pixels are premultiplied RGBA (the slint
/// image convention), which is what `TargetPixel::blend` expects.
fn composite_cursor<P: TargetPixel>(
    buffer: &mut [P],
    screen_width: usize,
    screen_height: usize,
    rotation: RenderingRotation,
    mouse_position: i_slint_core::api::PhysicalPosition,
    cursor_pixels: &[i_slint_core::graphics::Rgba8Pixel],
    cursor_width: usize,
    cursor_height: usize,
) {
    let (screen_width, screen_height) = (screen_width as i32, screen_height as i32);
    // The cursor's top-left sits at the pointer (mirroring the gl path,
    // which translates the item renderer to the position before drawing).
    let origin_x = mouse_position.x;
    let origin_y = mouse_position.y;
    for cy in 0..cursor_height as i32 {
        for cx in 0..cursor_width as i32 {
            let (bx, by) = window_to_buffer(
                rotation,
                screen_width,
                screen_height,
                origin_x + cx,
                origin_y + cy,
            );
            if bx < 0 || bx >= screen_width || by < 0 || by >= screen_height {
                continue;
            }
            let src = cursor_pixels[(cy * cursor_width as i32 + cx) as usize];
            let dst = &mut buffer[(by * screen_width + bx) as usize];
            // `Rgba8Pixel` is the rgb crate's RGBA8: r/g/b/a (premultiplied).
            dst.blend(PremultipliedRgbaColor {
                red: src.r,
                green: src.g,
                blue: src.b,
                alpha: src.a,
            });
        }
    }
}

/// Map a window-space pixel (physical, pre-rotation) to its position in the
/// screen-oriented frame buffer — the same mirror + transpose transform the
/// software renderer applies to the scene.
fn window_to_buffer(
    rotation: RenderingRotation,
    screen_width: i32,
    screen_height: i32,
    x: i32,
    y: i32,
) -> (i32, i32) {
    let (mut x, mut y) = (x, y);
    if matches!(rotation, RenderingRotation::Rotate270 | RenderingRotation::Rotate180) {
        x = screen_width - 1 - x;
    }
    if matches!(rotation, RenderingRotation::Rotate90 | RenderingRotation::Rotate180) {
        y = screen_height - 1 - y;
    }
    if matches!(rotation, RenderingRotation::Rotate90 | RenderingRotation::Rotate270) {
        std::mem::swap(&mut x, &mut y);
    }
    (x, y)
}
