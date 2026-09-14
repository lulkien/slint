// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore sgc
//! The sgc session: the backend's own connection to the
//! simple-graphics-controller daemon.
//!
//! `Backend::build` connects to the daemon's abstract socket `@sgc`, acquires
//! the DRM card lease this backend renders on, and — with the `libinput`
//! feature — every input device the daemon advertises (best-effort, see
//! [`SgcSession::connect_and_acquire`]); the event loop pumps the session from
//! then on. The app never sees any of this: picking the linuxsgc backend IS
//! the sgc connection (sgc or die — no direct device open, no fallback).

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::time::Duration;

use i_slint_core::platform::PlatformError;
#[cfg(feature = "libinput")]
use libsgc_rs::InputResource;
use libsgc_rs::{Resource, SgcClient, SgcError, SgcEvent};

/// The acquired session: a live client plus the resources we hold — the DRM
/// card lease we render on and, with the `libinput` feature, the input
/// devices that arrived with it.
pub struct SgcSession {
    client: RefCell<SgcClient>,
    /// The DRM card lease we render on.
    pub resource: Resource,
    /// Card index of the lease (`Resource::Drm { card }`).
    pub card: u8,
    /// The acquired input devices (`Resource::Input(_)`), in acquisition
    /// order. Only acquired when the `libinput` feature is on: without it the
    /// backend cannot consume input, and holding devices it ignores would
    /// block other clients (the daemon's first-owner policy). Acquisition is
    /// best-effort — a keyboard-less UI is fine, the DRM lease is the only
    /// hard requirement — so failed input acquires are logged and the session
    /// continues without the device. A `RefCell` because a device plugged in
    /// while the app runs is acquired later (see [`SgcSession::adopt_advertised`]).
    #[cfg(feature = "libinput")]
    pub inputs: RefCell<Vec<Resource>>,
    /// Every resource the daemon has offered this session: the connect-time
    /// list plus every pushed one. A resource we were refused is asked for again
    /// only if it leaves the list and comes back — the daemon keeps no memory of
    /// the request and the policy that refused us would refuse us again, so
    /// re-asking on every push would only repeat the message and hammer the
    /// daemon's log.
    #[cfg(feature = "libinput")]
    offered: RefCell<Vec<Resource>>,
}

impl SgcSession {
    /// Connect to the daemon and acquire the resources: the DRM lease the
    /// backend renders on, plus every input device the daemon advertises.
    ///
    /// Card selection: the FIRST DRM card the daemon advertises is acquired.
    /// Limitation: if that card cannot be used (acquire denied/blocked) the
    /// backend fails instead of trying the next advertised card — retrying
    /// further cards is future work.
    ///
    /// Fails (PlatformError) whenever the daemon is unreachable, advertises no
    /// DRM card, or denies the DRM acquire — the backend dies on startup.
    /// Acquiring the advertised input devices is part of the `libinput`
    /// feature and best-effort: a denied/failed input acquire only logs (the
    /// daemon may have granted the device to another client) and the session
    /// continues without it.
    pub fn connect_and_acquire() -> Result<Self, PlatformError> {
        let (client, advertised) = SgcClient::connect().map_err(sgc_err)?;

        let (card, resource) = advertised
            .iter()
            .find_map(|r| match r {
                Resource::Drm { card } => Some((*card, r.clone())),
                _ => None,
            })
            .ok_or_else(|| {
                PlatformError::Other(
                    "the @sgc daemon advertised no DRM card; nothing to render on".into(),
                )
            })?;
        println!("linuxsgc: acquiring Drm{{ card: {card} }} from @sgc...");

        let mut client = client;
        client.acquire(resource.clone()).map_err(sgc_err)?;
        println!("linuxsgc: lease for {resource:?} granted");

        // Input devices ride along with the lease when input support is
        // compiled in (feature `libinput`): acquire every advertised one so
        // the window can later receive pointer/keyboard/touch events on the
        // granted fds. Best-effort — a failed acquire must not kill the UI
        // (a keyboard-less app is still usable), but a DENIAL is permanent for
        // this process: the daemon keeps no memory of the request and the
        // protocol has no "tell me when it is free", so nothing re-asks for
        // us. That is worth saying out loud — the alternative is a kiosk that
        // silently has no keyboard. Without the feature the session holds only
        // the lease: devices it cannot consume must stay available to other
        // clients.
        #[cfg(feature = "libinput")]
        let inputs = {
            let mut inputs = Vec::new();
            for input in &advertised {
                let Resource::Input(_) = input else {
                    continue;
                };
                println!("linuxsgc: acquiring {input:?} from @sgc...");
                match client.acquire(input.clone()) {
                    Ok(()) => {
                        println!("linuxsgc: {input:?} granted");
                        inputs.push(input.clone());
                    }
                    Err(err) => report_input_refusal(input, &err),
                }
            }
            inputs
        };

        Ok(Self {
            client: RefCell::new(client),
            resource,
            card,
            #[cfg(feature = "libinput")]
            inputs: RefCell::new(inputs),
            #[cfg(feature = "libinput")]
            offered: RefCell::new(advertised),
        })
    }

    /// Acquire `resources` this session does not already hold, and return the
    /// ones granted now (the caller hands each to libinput).
    ///
    /// `why` is the reason shown in the log: the two callers below are the two
    /// ways a device reaches an app that is already running, and seeing which
    /// one asked is what makes a board log readable.
    #[cfg(feature = "libinput")]
    fn acquire_inputs(
        &self,
        resources: impl IntoIterator<Item = Resource>,
        why: &str,
    ) -> Vec<Resource> {
        let wanted: Vec<Resource> = resources
            .into_iter()
            .filter(|resource| matches!(resource, Resource::Input(_)))
            .filter(|resource| !self.inputs.borrow().contains(resource))
            .collect();
        if wanted.is_empty() {
            return Vec::new();
        }

        let mut client = self.client.borrow_mut();
        let mut inputs = self.inputs.borrow_mut();
        let mut granted = Vec::new();
        for input in wanted {
            println!("linuxsgc: acquiring {input:?} from @sgc ({why})...");
            match client.acquire(input.clone()) {
                Ok(()) => {
                    println!("linuxsgc: {input:?} granted");
                    inputs.push(input.clone());
                    granted.push(input);
                }
                Err(err) => report_input_refusal(&input, &err),
            }
        }
        granted
    }

    /// Adopt a resource list the daemon pushed after we connected: acquire the
    /// input devices we have not been offered before, and return the ones
    /// granted now (the caller hands them to libinput).
    ///
    /// This is how a device plugged in AFTER the app started reaches it — the
    /// connect-time view was a snapshot, and the daemon pushes its list whenever
    /// it changes. Two things need nothing here:
    ///
    /// - a resource that leaves the list: the daemon suspends it (its holder
    ///   keeps it — we are that holder) or revokes it ([`SgcEvent::Revoked`]),
    ///   and libinput reports `DEVICE_REMOVED` for a device we lose either way;
    /// - a resource that comes back on the list: we still hold it, so the daemon
    ///   re-grants it to us on its own and the acquire below skips it.
    ///
    /// "Not offered before" is the guard that keeps a REFUSAL from being
    /// repeated: the daemon keeps no memory of a request and the policy that
    /// refused one resource refuses it again, so re-asking on every push would
    /// only refill the log. [`Self::reacquire_inputs`] deliberately breaks that
    /// rule, and says why.
    #[cfg(feature = "libinput")]
    pub fn adopt_advertised(&self, advertised: &[Resource]) -> Vec<Resource> {
        let new: Vec<Resource> = advertised
            .iter()
            .filter(|resource| !self.offered.borrow().contains(resource))
            .cloned()
            .collect();
        *self.offered.borrow_mut() = advertised.to_vec();
        self.acquire_inputs(new, "appeared while running")
    }

    /// Ask for every input device the daemon currently advertises, because this
    /// app is BACK ON SCREEN after a preemption and the devices it held were
    /// revoked with its seat.
    ///
    /// The engine re-grants the display, never the devices that went with it
    /// (they are the client's to acquire again, in that order: asking while
    /// holding no display is exactly what the daemon denies). This is the app's
    /// half of a seat handover.
    ///
    /// It bypasses the "have I been offered this before" rule on purpose: that
    /// rule exists to stop a refusal being repeated on every push, not to stop a
    /// legitimate re-ask after the app lost and regained the screen — where the
    /// device is very likely free now, and where not asking means an app that is
    /// visible but has no pointer or keyboard.
    #[cfg(feature = "libinput")]
    pub fn reacquire_inputs(&self) -> Vec<Resource> {
        self.acquire_inputs(self.offered.borrow().clone(), "the display is back")
    }

    /// Forget an input resource the daemon revoked: this app no longer holds it,
    /// so it may be asked for again. A SUSPENDED device sends no revoke (the
    /// holder keeps it) and therefore never lands here.
    #[cfg(feature = "libinput")]
    pub fn forget_input(&self, resource: &Resource) {
        self.inputs.borrow_mut().retain(|held| held != resource);
    }

    /// Non-blocking protocol pump: returns one event if the server sent one,
    /// `Ok(None)` when nothing is pending (call again later). The protocol
    /// acks (`Release` on revoke, `Ack` on grant) are sent by the library
    /// before the event is returned.
    ///
    /// `Err` means the connection is over (daemon died/restarted): the lease
    /// is dead with it, so the backend must stop — there is nothing to render
    /// on and no way to recover without the daemon.
    pub fn pump(&self) -> Result<Option<SgcEvent>, PlatformError> {
        // poll(2) once: the event loop wakes us on a timer anyway, and events
        // that arrived since the last pump are all still buffered.
        self.client.borrow_mut().pump(Some(Duration::ZERO)).map_err(sgc_err)
    }

    /// A fresh dup of the currently held fd for `resource` (owned by the
    /// caller). Used at startup to seed the display stack (`Drm`) and the
    /// input registry (`Input(_)`); re-grants arrive as [`SgcEvent::Granted`]
    /// fds through [`SgcSession::pump`].
    pub fn fd(&self, resource: &Resource) -> Result<OwnedFd, PlatformError> {
        self.client.borrow().fd(resource).map_err(sgc_err)
    }
}

fn sgc_err(err: SgcError) -> PlatformError {
    PlatformError::Other(format!("sgc: {err}"))
}

/// Say why an input device did not arrive, and what it costs the app.
///
/// A denial is permanent for this process — the daemon keeps no memory of the
/// request and the protocol has no "tell me when it is free" — and it is only
/// reachable under a policy that never preempts (`first-owner`); under
/// fair-queue a newcomer takes the device over instead. So the message names the
/// device, the daemon's reason, the events the app will never see, and both ways
/// out.
#[cfg(feature = "libinput")]
fn report_input_refusal(input: &Resource, err: &SgcError) {
    let kind = match input {
        Resource::Input(InputResource::Mouse(_)) => "pointer",
        Resource::Input(InputResource::Keyboard(_)) => "keyboard",
        Resource::Input(InputResource::Touch(_)) => "touch",
        _ => "input",
    };
    match err {
        SgcError::Denied { reason } => eprintln!(
            "linuxsgc: cannot take {input:?} — the daemon denied it ({reason}). \
             This app will receive no {kind} events for its whole lifetime: another \
             client holds the device and the daemon's policy is first-owner, which \
             denies a newcomer rather than preempting the holder. Restart this app \
             once that client releases the device, or run @sgc with its default \
             fair-queue policy, where a newcomer preempts the holder instead"
        ),
        err => eprintln!(
            "linuxsgc: cannot take {input:?} — {err}. This app will receive no {kind} events"
        ),
    }
}
