// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore sgc
//! The sgc session: the backend's own connection to the
//! simple-graphics-controller daemon.
//!
//! `Backend::build` connects to the daemon's abstract socket `@sgc`, acquires
//! the DRM card lease this backend renders on, and the event loop pumps the
//! session from then on. The app never sees any of this: picking the linuxsgc
//! backend IS the sgc connection (sgc or die — no direct device open, no
//! fallback).

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::time::Duration;

use i_slint_core::platform::PlatformError;
use libsgc_rs::{Resource, SgcClient, SgcError, SgcEvent};

/// The acquired session: a live client plus the single resource we render on.
pub struct SgcSession {
    client: RefCell<SgcClient>,
    /// The DRM card lease we hold.
    pub resource: Resource,
    /// Card index of the lease (`Resource::Drm { card }`).
    pub card: u8,
}

impl SgcSession {
    /// Connect to the daemon and acquire the lease.
    ///
    /// Card selection: the FIRST DRM card the daemon advertises is acquired.
    /// Limitation: if that card cannot be used (acquire denied/blocked) the
    /// backend fails instead of trying the next advertised card — retrying
    /// further cards is future work.
    ///
    /// Fails (PlatformError) whenever the daemon is unreachable, advertises no
    /// DRM card, or denies the acquire — the backend dies on startup.
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

        Ok(Self { client: RefCell::new(client), resource, card })
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

    /// A fresh dup of the currently held lease fd (owned by the caller). Used
    /// at startup to seed the display stack; re-grants arrive as
    /// [`SgcEvent::Granted`] fds through [`SgcSession::pump`].
    pub fn fd(&self) -> Result<OwnedFd, PlatformError> {
        self.client.borrow().fd(&self.resource).map_err(sgc_err)
    }
}

fn sgc_err(err: SgcError) -> PlatformError {
    PlatformError::Other(format!("sgc: {err}"))
}
