// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore dumbbuffer
use std::rc::Rc;

use i_slint_core::platform::PlatformError;

pub trait SoftwareBufferDisplay {
    fn size(&self) -> (u32, u32);
    fn map_back_buffer(
        &self,
        callback: &mut dyn FnMut(
            &'_ mut [u8],
            u8,
            drm::buffer::DrmFourcc,
        ) -> Result<(), PlatformError>,
    ) -> Result<(), PlatformError>;
    fn as_presenter(self: Rc<Self>) -> Rc<dyn super::Presenter>;
}

mod dumbbuffer;

pub fn negotiate_format(
    renderer_formats: &[drm::buffer::DrmFourcc],
    display_formats: &[drm::buffer::DrmFourcc],
) -> Option<drm::buffer::DrmFourcc> {
    renderer_formats
        .iter()
        .find(|&&renderer_format| display_formats.contains(&renderer_format))
        .copied()
}

pub fn new(
    device_opener: &crate::DeviceOpener,
    renderer_formats: &[drm::buffer::DrmFourcc],
) -> Result<Rc<dyn SoftwareBufferDisplay>, PlatformError> {
    // sgc fork: the display is ALWAYS the DRM device the renderer was given —
    // a lease fd granted by the sgc daemon. No /dev/fb0 fallback: rendering
    // on anything but the granted device is not an option (sgc or die).
    dumbbuffer::DumbBufferDisplay::new(device_opener, renderer_formats)
}
