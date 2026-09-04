// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore drmoutput fullscreenwindowadapter sgc
#![doc = include_str!("README.md")]
#![doc(html_logo_url = "https://slint.dev/logo/slint-logo-square-light.svg")]

//! sgc fork of the linuxkms backend (Linux only; trimmed to the renderers we
//! use). See the crate header in Cargo.toml.

mod fullscreenwindowadapter;
mod calloop_backend;
mod display;

use std::os::fd::OwnedFd;

type DeviceOpener<'a> = dyn Fn(&std::path::Path) -> Result<std::rc::Rc<OwnedFd>, i_slint_core::platform::PlatformError>
    + 'a;

#[cfg(feature = "drm")]
mod drmoutput;

mod renderer {
    use i_slint_core::platform::PlatformError;

    use crate::fullscreenwindowadapter::FullscreenRenderer;

    #[cfg(feature = "renderer-femtovg")]
    pub mod femtovg;

    #[cfg(feature = "renderer-software")]
    pub mod sw;

    pub fn try_femtovg_then_software(
        device_opener: &crate::DeviceOpener,
        requested_graphics_api: Option<&i_slint_core::graphics::RequestedGraphicsAPI>,
    ) -> Result<Box<dyn FullscreenRenderer>, PlatformError> {
        #[allow(unused)]
        type FactoryFn = fn(
            &crate::DeviceOpener,
            Option<&i_slint_core::graphics::RequestedGraphicsAPI>,
        ) -> Result<Box<(dyn FullscreenRenderer)>, PlatformError>;

        let renderers = [
            #[cfg(feature = "renderer-femtovg")]
            ("FemtoVG", femtovg::FemtoVGRendererAdapter::new as FactoryFn),
            #[cfg(feature = "renderer-software")]
            ("Software", sw::SoftwareRendererAdapter::new as FactoryFn),
            ("", |_, _| Err(PlatformError::NoPlatform)),
        ];

        let mut renderer_errors: Vec<String> = Vec::new();
        for (name, factory) in renderers {
            match factory(device_opener, requested_graphics_api) {
                Ok(renderer) => return Ok(renderer),
                Err(err) => {
                    renderer_errors.push(if !name.is_empty() {
                        format!("Error from {} renderer: {}", name, err)
                    } else {
                        "No renderers configured.".into()
                    });
                }
            }
        }

        Err(PlatformError::Other(format!(
            "Could not initialize any renderer for the LinuxSGC backend.\n{}",
            renderer_errors.join("\n")
        )))
    }
}

// sgc fork: exported so apps can keep an `Rc<Backend>` (revoke/regrant API) and
// type event-loop hooks against the loop data.
pub use calloop_backend::{Backend, LoopData};

use i_slint_core::api::PlatformError;

#[derive(Default)]
pub struct BackendBuilder {
    pub(crate) renderer_name: Option<String>,
    pub(crate) requested_graphics_api: Option<i_slint_core::graphics::RequestedGraphicsAPI>,
    /// Input via libinput (opt-in feature). Without it the backend renders but
    /// receives no input events.
    #[cfg(feature = "libinput")]
    pub(crate) libinput_event_hook: Option<Box<dyn Fn(&input::Event) -> bool>>,
    /// sgc lease (WIP): render on a pre-opened DRM fd (e.g. a lease granted by a
    /// controller daemon) instead of opening /dev/dri/cardN directly.
    pub(crate) drm_fd: Option<(u8, std::rc::Rc<OwnedFd>)>,
}

impl BackendBuilder {
    pub fn with_renderer_name(mut self, name: String) -> Self {
        self.renderer_name = Some(name);
        self
    }

    pub fn request_graphics_api(
        mut self,
        graphics_api: i_slint_core::graphics::RequestedGraphicsAPI,
    ) -> Self {
        self.requested_graphics_api = Some(graphics_api);
        self
    }

    #[cfg(feature = "libinput")]
    pub fn with_libinput_event_hook(
        mut self,
        event_hook: Box<dyn Fn(&input::Event) -> bool>,
    ) -> Self {
        self.libinput_event_hook = Some(event_hook);
        self
    }

    /// sgc lease (WIP): render on a pre-opened DRM fd for `/dev/dri/card{card_index}`
    /// instead of opening the device directly. `fd` may be a DRM lease: everything
    /// downstream (connector/mode probing, gbm, dumb buffers, page flips) works on a
    /// lease fd without DRM master. The fd must stay valid for the backend's lifetime.
    pub fn with_drm_device(mut self, card_index: u8, fd: OwnedFd) -> Self {
        self.drm_fd = Some((card_index, std::rc::Rc::new(fd)));
        self
    }

    pub fn build(self) -> Result<Backend, PlatformError> {
        Backend::build(self)
    }
}

#[doc(hidden)]
pub type NativeWidgets = ();
#[doc(hidden)]
pub type NativeGlobals = ();
#[doc(hidden)]
pub const HAS_NATIVE_STYLE: bool = false;
#[doc(hidden)]
pub mod native_widgets {}
