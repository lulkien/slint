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
    /// continues without the device.
    #[cfg(feature = "libinput")]
    pub inputs: Vec<Resource>,
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
        // granted fds. Best-effort — a failed acquire must not kill the UI;
        // log it and continue without the device. Without the feature the
        // session holds only the lease: devices it cannot consume must stay
        // available to other clients.
        #[cfg(feature = "libinput")]
        let inputs = {
            let mut inputs = Vec::new();
            for input in &advertised {
                if !matches!(input, Resource::Input(_)) {
                    continue;
                }
                println!("linuxsgc: acquiring {input:?} from @sgc...");
                match client.acquire(input.clone()) {
                    Ok(()) => {
                        println!("linuxsgc: {input:?} granted");
                        inputs.push(input.clone());
                    }
                    Err(err) => eprintln!(
                        "linuxsgc: acquire of {input:?} failed — continuing without it: {err}"
                    ),
                }
            }
            inputs
        };

        Ok(Self {
            client: RefCell::new(client),
            resource,
            card,
            #[cfg(feature = "libinput")]
            inputs,
        })
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
