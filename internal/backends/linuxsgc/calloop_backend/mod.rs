// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore CLOEXEC GETFL NOCTTY NONBLOCK sgc
// sgc fork of the linuxkms calloop backend, trimmed to Linux, no libseat, no
// libinput-by-default (headless lease clients). The display is ALWAYS the DRM
// lease the backend acquired from the @sgc daemon: `Backend::build` connects
// and acquires (sgc or die — no direct /dev/dri open, no fbdev fallback), and
// the event loop pumps the session. A revoke suspends rendering until the
// lease is re-granted, on which the display stack is rebuilt on the fresh fd.

use std::cell::{Cell, RefCell};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use calloop::EventLoop;
use i_slint_core::platform::PlatformError;
use libsgc_rs::{Resource, SgcEvent};

use crate::fullscreenwindowadapter::FullscreenWindowAdapter;
use crate::sgc::SgcSession;

#[cfg(feature = "libinput")]
mod input;
#[cfg(feature = "libinput")]
mod input_shared;

/// How often the sgc socket is pumped (non-blocking poll). Far inside the
/// daemon's 5s revoke/ack grace period; cheap enough to run always.
const SGC_PUMP_INTERVAL: Duration = Duration::from_millis(50);

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

/// Everything the event loop and the window adapter share. All of it lives
/// behind an `Rc` so the sgc pump (a calloop source with 'static captures)
/// can drive suspend/rebuild without owning the `Backend`.
struct SharedState {
    window: RefCell<Option<Rc<FullscreenWindowAdapter>>>,
    /// The current lease fd (a dup, Rc'd): seeded at `build` from the
    /// acquire, swapped on re-grant, cleared on revoke. The renderer display
    /// stack is (re)built on whatever is in here.
    drm_fd: Rc<RefCell<Option<Rc<OwnedFd>>>>,
    /// Card index of the lease; the only `/dev/dri/cardN` path the backend
    /// will ever open.
    card: u8,
    /// While set, the window adapter skips rendering (lease revoked, pending
    /// re-grant). Shared with the adapter.
    suspended: Rc<Cell<bool>>,
    /// The input side of the session: the shared libinput path context over
    /// the granted devices. The pump routes input grant/revoke events here
    /// (devices are added/removed on this thread). Featureless builds hold
    /// no input resources, so they never see an input event.
    #[cfg(feature = "libinput")]
    input_state: Rc<input_shared::InputState>,
}

/// Device opener that hands out the current sgc lease fd and refuses anything
/// else: rendering happens on the granted device and on nothing else.
/// Rebuilt on every call so a swapped fd (re-grant) is picked up by the next
/// renderer/display-stack init.
impl SharedState {
    fn device_accessor(&self) -> Box<crate::DeviceOpener<'static>> {
        let card_name = format!("card{}", self.card);
        let drm_fd = self.drm_fd.clone();
        Box::new(move |device: &std::path::Path| -> Result<Rc<OwnedFd>, PlatformError> {
            if device.file_name().and_then(|name| name.to_str()) != Some(card_name.as_str()) {
                return Err(format!(
                    "Refusing to open {}: rendering only on the sgc lease fd for {}",
                    device.display(),
                    card_name
                )
                .into());
            }
            let fd = drm_fd.borrow().clone().ok_or_else(|| {
                PlatformError::Other(format!(
                    "No lease fd held for {} (revoked?): nothing to render on",
                    device.display()
                ))
            })?;
            // For polling for drm::control::Event::PageFlip we need a blocking
            // FD. The fd came over SCM_RIGHTS, sharing its open file
            // description with the daemon's copy — this clears O_NONBLOCK on
            // the shared description; the daemon must not rely on it.
            let fd_borrowed = fd.as_fd();
            let flags = nix::fcntl::fcntl(fd_borrowed, nix::fcntl::FcntlArg::F_GETFL)
                .map_err(|e| format!("Error getting file descriptor flags: {e}"))?;
            let mut flags = nix::fcntl::OFlag::from_bits_retain(flags);
            flags.remove(nix::fcntl::OFlag::O_NONBLOCK);
            nix::fcntl::fcntl(fd_borrowed, nix::fcntl::FcntlArg::F_SETFL(flags))
                .map_err(|e| format!("Error making device fd blocking: {e}"))?;
            Ok(fd)
        })
    }

    /// Apply one sgc event: DRM events drive suspend/rebuild of the display
    /// stack, input events add/remove devices in the shared libinput
    /// context. Routing is strict by resource kind — an input event never
    /// touches the drm fd slot and vice versa.
    fn on_sgc_event(&self, event: SgcEvent) -> Result<(), PlatformError> {
        match event {
            SgcEvent::Revoked { resource: resource @ Resource::Drm { card } } => {
                // This backend holds exactly one card; a revoke naming
                // another card is a protocol anomaly and must not suspend a
                // display stack we still own.
                if card != self.card {
                    eprintln!(
                        "linuxsgc: ignoring revoke of {resource:?} — this backend holds Drm{{ card: {} }}",
                        self.card
                    );
                    return Ok(());
                }
                // The library already sent the Release revoke-ack; the daemon
                // requeues us. Drop the fd slot and stop rendering: the old
                // display stack died with the lease and must not be touched.
                println!("linuxsgc: lease {resource:?} revoked — suspending until re-granted");
                self.suspended.set(true);
                *self.drm_fd.borrow_mut() = None;
            }
            SgcEvent::Granted { resource: resource @ Resource::Drm { card }, fd } => {
                if card != self.card {
                    eprintln!(
                        "linuxsgc: ignoring grant of {resource:?} — this backend holds Drm{{ card: {} }}",
                        self.card
                    );
                    return Ok(());
                }
                println!(
                    "linuxsgc: lease {resource:?} re-granted (fd {}) — rebuilding display stack",
                    fd.as_raw_fd()
                );
                *self.drm_fd.borrow_mut() = Some(Rc::new(fd));
                if let Some(adapter) = self.window.borrow().clone() {
                    let accessor = self.device_accessor();
                    // Software renderer: rebuilds its whole display stack on
                    // the fresh fd. The GL renderer cannot be rebuilt
                    // in-process (its EGL/GL context dies with the lease fd),
                    // so this errors and the error ends the event loop —
                    // documented limitation of the femtovg flavor.
                    adapter.rebuild_renderer(&accessor)?;
                }
                self.suspended.set(false);
            }
            // Input resources: add/remove the device in the shared libinput
            // context. Both run on the event-loop thread (pump callback).
            // Featureless builds acquire no input resources, so an input
            // event can never name one they hold.
            #[cfg(feature = "libinput")]
            SgcEvent::Revoked { resource: resource @ Resource::Input(_) } => {
                self.input_state.on_revoked(&resource);
            }
            #[cfg(feature = "libinput")]
            SgcEvent::Granted { resource: resource @ Resource::Input(_), fd } => {
                self.input_state.on_granted(resource, fd);
            }
            other => {
                eprintln!("linuxsgc: ignoring {other:?} — not a resource this backend holds");
            }
        }
        Ok(())
    }
}

/// Drain the sgc socket: handle every event the daemon has queued.
/// `Err` = the connection is over (daemon died/restarted) — the lease died
/// with it; the caller must stop.
fn pump_sgc(shared: &SharedState, session: &SgcSession) -> Result<(), PlatformError> {
    loop {
        match session.pump()? {
            Some(event) => shared.on_sgc_event(event)?,
            None => return Ok(()),
        }
    }
}

pub struct Backend {
    shared: Rc<SharedState>,
    /// The sgc session (client + acquired lease). Owned for the backend's
    /// lifetime; dropped with it, which closes the socket and lets the daemon
    /// reclaim the card.
    sgc_session: Rc<SgcSession>,
    user_event_receiver: RefCell<Option<calloop::channel::Channel<Box<dyn FnOnce() + Send>>>>,
    proxy: Proxy,
    /// Fatal error stashed by the sgc pump (session lost, rebuild failed);
    /// ends the event loop with an error instead of Ok.
    fatal: Rc<RefCell<Option<PlatformError>>>,
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
    /// The input side of the sgc session: the libinput path context over the
    /// daemon-granted devices (created at `build`, seeded from the session's
    /// acquired inputs, consumed by the libinput dispatch source).
    #[cfg(feature = "libinput")]
    input_state: Rc<input_shared::InputState>,
}

impl Backend {
    pub fn build(builder: crate::BackendBuilder) -> Result<Self, PlatformError> {
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

        // sgc or die: connect to the daemon and acquire the lease now, so a
        // missing/denying daemon fails the app at startup with a clear error.
        let session = SgcSession::connect_and_acquire()?;
        let card = session.card;
        let fd = Rc::new(session.fd(&session.resource)?);

        // The input side of the session: a libinput path context over every
        // granted input device. Devices are duped and resolved to their
        // /dev/input paths here (no libinput interaction yet); libinput only
        // sees them at run_event_loop start, on the loop thread.
        #[cfg(feature = "libinput")]
        let input_state = {
            let input_state = input_shared::InputState::new();
            input_state.seed_from_session(&session);
            input_state
        };

        Ok(Backend {
            shared: Rc::new(SharedState {
                window: RefCell::new(None),
                drm_fd: Rc::new(RefCell::new(Some(fd))),
                card,
                suspended: Rc::new(Cell::new(false)),
                #[cfg(feature = "libinput")]
                input_state: input_state.clone(),
            }),
            sgc_session: Rc::new(session),
            user_event_receiver: RefCell::new(Some(user_event_receiver)),
            proxy: Proxy::new(user_event_sender),
            fatal: Rc::new(RefCell::new(None)),
            renderer_factory,
            requested_graphics_api: builder.requested_graphics_api,
            sel_clipboard: Default::default(),
            clipboard: Default::default(),
            #[cfg(feature = "libinput")]
            libinput_event_hook: builder.libinput_event_hook,
            #[cfg(feature = "libinput")]
            input_state,
        })
    }
}

impl i_slint_core::platform::Platform for Backend {
    fn create_window_adapter(
        &self,
    ) -> Result<std::rc::Rc<dyn i_slint_core::window::WindowAdapter>, PlatformError> {
        // This could be per-screen, once we support multiple outputs
        let rotation =
            std::env::var("SLINT_KMS_ROTATION").map_or(Ok(Default::default()), |rot_str| {
                rot_str
                    .as_str()
                    .try_into()
                    .map_err(|e| format!("Failed to parse SLINT_KMS_ROTATION: {e}"))
            })?;

        let adapter = FullscreenWindowAdapter::new(
            (self.renderer_factory)(
                &self.shared.device_accessor(),
                self.requested_graphics_api.as_ref(),
            )?,
            rotation,
            self.shared.suspended.clone(),
        )?;

        *self.shared.window.borrow_mut() = Some(adapter.clone());

        Ok(adapter)
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let mut event_loop: EventLoop<()> =
            EventLoop::try_new().map_err(|e| format!("Error creating event loop: {}", e))?;

        let loop_signal = event_loop.get_signal();

        *self.proxy.loop_signal.lock().unwrap() = Some(loop_signal.clone());
        if let Some(adapter) = self.shared.window.borrow().as_ref() {
            adapter.set_loop_signal(loop_signal.clone());
        }
        let quit_loop = self.proxy.quit_loop.clone();

        #[cfg(feature = "libinput")]
        let mouse_position_property = input::LibInputHandler::init(
            &self.shared.window,
            &event_loop.handle(),
            &self.libinput_event_hook,
            self.input_state.clone(),
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

        // Drain whatever the daemon sent between `build` (acquire) and the
        // loop starting — e.g. an immediate revoke — before the first frame.
        pump_sgc(&self.shared, &self.sgc_session)?;

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

        // sgc pump: poll the session socket on a short timer (the daemon is
        // purely event-driven; nothing wakes us otherwise). Revoke/regrant are
        // handled here, on the event-loop thread — suspend stops rendering
        // before the next frame, a re-grant rebuilds the display stack and
        // requests a redraw. A lost connection or failed rebuild is fatal.
        {
            let shared = self.shared.clone();
            let session = self.sgc_session.clone();
            let fatal = self.fatal.clone();
            let quit_loop = quit_loop.clone();
            let wakeup = loop_signal.clone();
            event_loop
                .handle()
                .insert_source(calloop::timer::Timer::from_duration(SGC_PUMP_INTERVAL), {
                    move |_deadline, _, _| {
                        if let Err(err) = pump_sgc(&shared, &session) {
                            eprintln!("linuxsgc: sgc session lost: {err}");
                            *fatal.borrow_mut() = Some(err);
                            quit_loop.store(true, std::sync::atomic::Ordering::Release);
                            wakeup.wakeup();
                        }
                        calloop::timer::TimeoutAction::ToDuration(SGC_PUMP_INTERVAL)
                    }
                })
                .map_err(|e: calloop::InsertError<calloop::timer::Timer>| {
                    format!("Error registering sgc pump source: {e}")
                })?;
        }

        quit_loop.store(false, std::sync::atomic::Ordering::Release);

        while !quit_loop.load(std::sync::atomic::Ordering::Acquire) {
            i_slint_core::platform::update_timers_and_animations();

            // Only after updating the animation tick, invoke callbacks from invoke_from_event_loop(). They
            // might set animated properties, which requires an up-to-date start time.
            for callback in callbacks_to_invoke_per_iteration.take().into_iter() {
                callback();
            }

            if let Some(adapter) = self.shared.window.borrow().as_ref() {
                adapter.clone().render_if_needed(mouse_position_property.as_ref())?;
            };

            let next_timeout = i_slint_core::platform::duration_until_next_timer_update();
            event_loop
                .dispatch(next_timeout, &mut ())
                .map_err(|e| format!("Error dispatch events: {e}"))?;
        }

        // A fatal sgc error (daemon gone, rebuild failed) takes precedence over
        // a clean app quit.
        if let Some(err) = self.fatal.borrow_mut().take() {
            return Err(err);
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
