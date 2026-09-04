// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore CLOEXEC GETFL NOCTTY NONBLOCK sgc
// sgc fork of the linuxkms calloop backend, trimmed to Linux, no libseat, no
// libinput (headless lease clients). Upstream's seat/input handling is gone;
// the device is either the injected DRM (lease) fd or opened directly.

use std::cell::{Cell, RefCell};
use std::fs::OpenOptions;
use std::os::fd::OwnedFd;
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use calloop::EventLoop;
use i_slint_core::platform::PlatformError;

use crate::BackendBuilder;
use crate::fullscreenwindowadapter::FullscreenWindowAdapter;

#[cfg(feature = "libinput")]
mod input;

#[derive(Clone)]
struct Proxy {
    loop_signal: Arc<Mutex<Option<calloop::LoopSignal>>>,
    quit_loop: Arc<AtomicBool>,
    user_event_channel: Arc<Mutex<calloop::channel::Sender<Box<dyn FnOnce() + Send>>>>,
}

impl Proxy {
    fn new(event_channel: calloop::channel::Sender<Box<dyn FnOnce() + Send>>) -> Self {
        Self {
            loop_signal: Arc::new(Mutex::new(None)),
            quit_loop: Arc::new(AtomicBool::new(false)),
            user_event_channel: Arc::new(Mutex::new(event_channel)),
        }
    }
}

impl i_slint_core::platform::EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), i_slint_core::api::EventLoopError> {
        let signal = self.loop_signal.lock().unwrap();
        signal.as_ref().map_or_else(
            || Err(i_slint_core::api::EventLoopError::EventLoopTerminated),
            |signal| {
                self.quit_loop.store(true, std::sync::atomic::Ordering::Release);
                signal.wakeup();
                Ok(())
            },
        )
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), i_slint_core::api::EventLoopError> {
        let user_event_channel = self.user_event_channel.lock().unwrap();
        user_event_channel
            .send(event)
            .map_err(|_| i_slint_core::api::EventLoopError::EventLoopTerminated)
    }
}

type EventLoopHook =
    Box<dyn for<'a> FnOnce(&calloop::LoopHandle<'a, LoopData>) -> Result<(), PlatformError>>;

pub struct Backend {
    window: RefCell<Option<Rc<FullscreenWindowAdapter>>>,
    user_event_receiver: RefCell<Option<calloop::channel::Channel<Box<dyn FnOnce() + Send>>>>,
    proxy: Proxy,
    /// sgc fork: the injected DRM (lease) fd, in a shared slot so it can be
    /// swapped when a revoked lease is re-granted (see `set_drm_fd`).
    drm_fd: Rc<RefCell<Option<(u8, Rc<OwnedFd>)>>>,
    /// While set, the window adapter skips rendering (revoked lease pending
    /// re-grant). Shared with the adapter.
    suspended: Rc<Cell<bool>>,
    /// sgc fork: sources the app wants registered in the backend's event loop
    /// (e.g. a poll source on the resource-controller socket). Run once, at
    /// the top of `run_event_loop`.
    event_loop_hooks: RefCell<Vec<EventLoopHook>>,
    renderer_factory:
        fn(
            &crate::DeviceOpener,
            Option<&i_slint_core::graphics::RequestedGraphicsAPI>,
        )
            -> Result<Box<dyn crate::fullscreenwindowadapter::FullscreenRenderer>, PlatformError>,
    requested_graphics_api: Option<i_slint_core::graphics::RequestedGraphicsAPI>,
    sel_clipboard: RefCell<Option<String>>,
    clipboard: RefCell<Option<String>>,
    #[cfg(feature = "libinput")]
    libinput_event_hook: Option<Box<dyn Fn(&::input::Event) -> bool>>,
}

impl Backend {
    pub fn build(builder: BackendBuilder) -> Result<Self, PlatformError> {
        let (user_event_sender, user_event_receiver) = calloop::channel::channel();

        let renderer_factory = match builder.renderer_name.as_deref() {
            #[cfg(feature = "renderer-femtovg")]
            Some("femtovg") => crate::renderer::femtovg::FemtoVGRendererAdapter::new,
            #[cfg(feature = "renderer-software")]
            Some("software") => crate::renderer::sw::SoftwareRendererAdapter::new,
            None => crate::renderer::try_femtovg_then_software,
            Some(renderer_name) => {
                eprintln!(
                    "slint linuxsgc backend: unrecognized renderer {}, falling back default",
                    renderer_name
                );
                crate::renderer::try_femtovg_then_software
            }
        };

        Ok(Backend {
            window: Default::default(),
            user_event_receiver: RefCell::new(Some(user_event_receiver)),
            proxy: Proxy::new(user_event_sender),
            drm_fd: Rc::new(RefCell::new(builder.drm_fd)),
            suspended: Rc::new(Cell::new(false)),
            event_loop_hooks: RefCell::new(Vec::new()),
            renderer_factory,
            requested_graphics_api: builder.requested_graphics_api,
            sel_clipboard: Default::default(),
            clipboard: Default::default(),
            #[cfg(feature = "libinput")]
            libinput_event_hook: builder.libinput_event_hook,
        })
    }
}

// sgc fork: device opener that renders on the currently injected DRM (lease)
// fd (or opens the device directly when none is injected), plus the
// revoke/regrant API. The opener is rebuilt on every call so a swapped fd
// (re-grant) is picked up by the next renderer rebuild.
impl Backend {
    fn device_accessor(&self) -> Box<crate::DeviceOpener<'static>> {
        if let Some((card_index, fd)) = self.drm_fd.borrow().as_ref() {
            let card_index = *card_index;
            let card_name = format!("card{card_index}");
            let fd = fd.clone();
            return Box::new(
                move |device: &std::path::Path| -> Result<Rc<OwnedFd>, PlatformError> {
                    if device.file_name().and_then(|name| name.to_str())
                        != Some(card_name.as_str())
                    {
                        return Err(format!(
                            "Refusing to open {}: rendering on the injected DRM fd for card{}",
                            device.display(),
                            card_index
                        )
                        .into());
                    }
                    // For polling for drm::control::Event::PageFlip we need a
                    // blocking FD. The fd is a dup of the injector's — this
                    // clears O_NONBLOCK on the shared open file description; the
                    // injector must not rely on it.
                    let fd_borrowed = fd.as_fd();
                    let flags = nix::fcntl::fcntl(fd_borrowed, nix::fcntl::FcntlArg::F_GETFL)
                        .map_err(|e| format!("Error getting file descriptor flags: {e}"))?;
                    let mut flags = nix::fcntl::OFlag::from_bits_retain(flags);
                    flags.remove(nix::fcntl::OFlag::O_NONBLOCK);
                    nix::fcntl::fcntl(fd_borrowed, nix::fcntl::FcntlArg::F_SETFL(flags))
                        .map_err(|e| format!("Error making device fd blocking: {e}"))?;
                    Ok(fd.clone())
                },
            );
        }

        Box::new(|device: &std::path::Path| -> Result<Rc<OwnedFd>, PlatformError> {
            let device = OpenOptions::new()
                .custom_flags((nix::fcntl::OFlag::O_NOCTTY | nix::fcntl::OFlag::O_CLOEXEC).bits())
                .read(true)
                .write(true)
                .open(device)
                .map(|file| file.into())
                .map_err(|e| format!("Error opening device {}: {e}", device.display()))?;

            Ok(Rc::new(device))
        })
    }

    /// sgc fork: swap the injected DRM fd for a fresh one (a re-granted lease
    /// after a revoke). Call [`Self::rebuild_renderer`] afterwards; rendering
    /// must be suspended (`set_suspended(true)`) in between.
    pub fn set_drm_fd(&self, card_index: u8, fd: OwnedFd) {
        *self.drm_fd.borrow_mut() = Some((card_index, Rc::new(fd)));
    }

    /// sgc fork: while suspended the window adapter renders nothing (a revoked
    /// lease fd must not be touched until it is re-granted and rebuilt).
    pub fn set_suspended(&self, suspended: bool) {
        self.suspended.set(suspended);
    }

    /// sgc fork: rebuild the window adapter's display stack on the CURRENT
    /// injected fd (call after `set_drm_fd` with a re-granted lease). Must run
    /// on the event-loop thread.
    pub fn rebuild_renderer(&self) -> Result<(), PlatformError> {
        let accessor = self.device_accessor();
        let Some(adapter) = self.window.borrow().clone() else {
            return Err(PlatformError::Other(
                "rebuild_renderer: no window adapter yet".into(),
            ));
        };
        adapter.rebuild_renderer(&accessor)
    }

    /// sgc fork: register a hook that runs once at the top of `run_event_loop`,
    /// with the loop handle — for inserting extra event sources (e.g. a poll
    /// source driving the resource-controller client).
    pub fn add_event_loop_hook(&self, hook: EventLoopHook) {
        self.event_loop_hooks.borrow_mut().push(hook);
    }
}

impl i_slint_core::platform::Platform for Backend {
    fn create_window_adapter(
        &self,
    ) -> Result<std::rc::Rc<dyn i_slint_core::window::WindowAdapter>, PlatformError> {
        let device_accessor = self.device_accessor();

        // This could be per-screen, once we support multiple outputs
        let rotation =
            std::env::var("SLINT_KMS_ROTATION").map_or(Ok(Default::default()), |rot_str| {
                rot_str
                    .as_str()
                    .try_into()
                    .map_err(|e| format!("Failed to parse SLINT_KMS_ROTATION: {e}"))
            })?;

        let renderer =
            (self.renderer_factory)(&device_accessor, self.requested_graphics_api.as_ref())?;
        let adapter =
            FullscreenWindowAdapter::new(renderer, rotation, self.suspended.clone())?;

        *self.window.borrow_mut() = Some(adapter.clone());

        Ok(adapter)
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let mut event_loop: EventLoop<LoopData> =
            EventLoop::try_new().map_err(|e| format!("Error creating event loop: {}", e))?;

        let loop_signal = event_loop.get_signal();

        *self.proxy.loop_signal.lock().unwrap() = Some(loop_signal.clone());
        if let Some(adapter) = self.window.borrow().as_ref() {
            adapter.set_loop_signal(loop_signal.clone());
        }
        let quit_loop = self.proxy.quit_loop.clone();

        #[cfg(feature = "libinput")]
        let mouse_position_property = input::LibInputHandler::init(
            &self.window,
            &event_loop.handle(),
            &self.libinput_event_hook,
        )?;

        // Without libinput there is no pointer to track, so the cursor property
        // stays empty for the lifetime of the loop.
        #[cfg(not(feature = "libinput"))]
        let mouse_position_property = Rc::pin(i_slint_core::Property::<
            Option<i_slint_core::api::LogicalPosition>,
        >::new(None));

        let Some(user_event_receiver) = self.user_event_receiver.borrow_mut().take() else {
            return Err("Re-entering the linuxsgc event loop is currently not supported"
                .to_string()
                .into());
        };

        // sgc fork: run the app-registered hooks (e.g. inserting a poll source
        // on the resource-controller socket) now that the loop handle exists.
        for hook in self.event_loop_hooks.borrow_mut().drain(..) {
            hook(&event_loop.handle())?;
        }

        let callbacks_to_invoke_per_iteration = Rc::new(RefCell::new(Vec::new()));

        event_loop
            .handle()
            .insert_source(user_event_receiver, {
                let callbacks_to_invoke_per_iteration = callbacks_to_invoke_per_iteration.clone();
                move |event, _, _| {
                    let calloop::channel::Event::Msg(callback) = event else { return };
                    // Remember the callbacks and invoke them after updating the animation tick
                    callbacks_to_invoke_per_iteration.borrow_mut().push(callback);
                }
            })
            .map_err(
                |e: calloop::InsertError<calloop::channel::Channel<Box<dyn FnOnce() + Send>>>| {
                    format!("Error registering user event channel source: {e}")
                },
            )?;

        let mut loop_data = LoopData::default();

        quit_loop.store(false, std::sync::atomic::Ordering::Release);

        while !quit_loop.load(std::sync::atomic::Ordering::Acquire) {
            i_slint_core::platform::update_timers_and_animations();

            // Only after updating the animation tick, invoke callbacks from invoke_from_event_loop(). They
            // might set animated properties, which requires an up-to-date start time.
            for callback in callbacks_to_invoke_per_iteration.take().into_iter() {
                callback();
            }

            if let Some(adapter) = self.window.borrow().as_ref() {
                adapter.clone().render_if_needed(mouse_position_property.as_ref())?;
            };

            let next_timeout = i_slint_core::platform::duration_until_next_timer_update();
            event_loop
                .dispatch(next_timeout, &mut loop_data)
                .map_err(|e| format!("Error dispatch events: {e}"))?;
        }

        Ok(())
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn i_slint_core::platform::EventLoopProxy>> {
        Some(Box::new(self.proxy.clone()))
    }

    fn clipboard_text(&self, clipboard: i_slint_core::platform::Clipboard) -> Option<String> {
        match clipboard {
            i_slint_core::platform::Clipboard::DefaultClipboard => self.clipboard.borrow().clone(),
            i_slint_core::platform::Clipboard::SelectionClipboard => {
                self.sel_clipboard.borrow().clone()
            }
            _ => None,
        }
    }
    fn set_clipboard_text(&self, text: &str, clipboard: i_slint_core::platform::Clipboard) {
        match clipboard {
            i_slint_core::platform::Clipboard::DefaultClipboard => {
                *self.clipboard.borrow_mut() = Some(text.into())
            }
            i_slint_core::platform::Clipboard::SelectionClipboard => {
                *self.sel_clipboard.borrow_mut() = Some(text.into())
            }
            _ => (),
        }
    }
}

#[derive(Default)]
pub struct LoopData {}
