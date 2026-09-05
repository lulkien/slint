// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore NONBLOCK sgc
//! The input side of the sgc session: the input devices the @sgc daemon
//! granted, fed into a libinput path context.
//!
//! The daemon opens /dev/input/eventN, classifies it and grants the fd over
//! SCM_RIGHTS — the backend never opens an input device itself (sgc-or-die
//! applies to input as much as to display). libinput's PATH backend fits
//! this shape: every device it opens goes through the `LibinputInterface`,
//! so its `open_restricted` ignores the path it is given and returns a dup
//! of the granted fd for the matching device instead. The path is only
//! libinput bookkeeping: the real /dev/input/eventN is resolved once, at
//! grant time, via readlink(/proc/self/fd/<fd>).
//!
//! Layout (single-threaded, event-loop thread):
//! - [`InputState`] owns the libinput context and the device registry. It is
//!   created at `Backend::build`, seeded with the session's acquired inputs,
//!   and shared (Rc) with the dispatch handler and the sgc pump routing.
//!   libinput is refcounted (`clone` = libinput_ref), so the handler and the
//!   routing work on clones of the context.
//! - [`InputRegistry`] is the granted-device table. The libinput context
//!   holds one clone of it as its open/close interface. Adding or removing a
//!   device re-enters the registry through the interface synchronously, so
//!   the registry is only ever borrowed for short snapshots, never across a
//!   libinput call.
//!
//! Fd semantics: the granted fd — and every dup of it — shares its open file
//! description with the daemon's fd, so fcntl flag changes (O_NONBLOCK) land
//! on both. The daemon reads nothing from the device after granting, so
//! making it non-blocking is harmless, but never assume per-fd flag
//! isolation.

use std::cell::RefCell;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use libsgc_rs::Resource;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, OFlag};

use crate::sgc::SgcSession;

/// One granted input device: the @sgc resource, the fd libinput reads from,
/// and the path libinput books the device under.
pub struct GrantedInput {
    /// The @sgc resource this device was granted as.
    pub resource: Resource,
    /// The registry's dup of the granted fd (the SgcClient owns the
    /// canonical until the daemon revokes). O_NONBLOCK is applied, see
    /// [`InputRegistry::add_granted`].
    fd: OwnedFd,
    /// The real device path (/dev/input/eventN), resolved at grant time via
    /// readlink(/proc/self/fd/<fd>).
    pub path: PathBuf,
    /// libinput's device handle once `path_add_device` succeeded. Kept so
    /// the device can be removed again on revoke — dropping the entry
    /// without `path_remove_device` would leak the device in libinput.
    device: Option<input::Device>,
}

/// The granted-device table, shared between the libinput interface (which
/// looks fds up by path while a device is opened), the seed path and the sgc
/// pump routing. Clone = another Rc to the same table.
#[derive(Clone, Default)]
pub struct InputRegistry(Rc<RefCell<Vec<GrantedInput>>>);

impl InputRegistry {
    /// Register one granted device. The caller hands over a fresh dup of the
    /// granted fd; this stores it, resolves the real device path and applies
    /// O_NONBLOCK. Failure (unresolvable path, duplicate) is logged and the
    /// device skipped — input is best-effort by design.
    fn add_granted(&self, resource: Resource, fd: OwnedFd) {
        // The dup shares the daemon's open file description, so
        // /proc/self/fd/<fd> resolves to the real /dev/input/eventN.
        let Ok(path) = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())) else {
            eprintln!(
                "linuxsgc: input: cannot resolve the device path of granted {resource:?} (fd {}) — skipping",
                fd.as_raw_fd()
            );
            return;
        };
        if !path.is_absolute() {
            eprintln!(
                "linuxsgc: input: granted {resource:?} resolved to non-device path {path:?} — skipping"
            );
            return;
        }

        // O_NONBLOCK is a file-description flag: it lands on the open file
        // description shared with the daemon's fd. Harmless here — the daemon
        // reads nothing from the device after granting (same class as the
        // drm fd) — but never assume per-fd flag isolation. FD_CLOEXEC is
        // per-fd and set on this dup only.
        if let Err(errno) = apply_open_flags(&fd, OFlag::O_NONBLOCK | OFlag::O_CLOEXEC) {
            eprintln!(
                "linuxsgc: input: cannot set fd flags of granted {resource:?} (errno {errno}) — skipping"
            );
            return;
        }

        let mut devices = self.0.borrow_mut();
        if devices.iter().any(|granted| granted.path == path) {
            eprintln!("linuxsgc: input: duplicate device path {path:?} — skipping");
            return;
        }
        println!("linuxsgc: input: registered granted {resource:?}: {}", path.display());
        devices.push(GrantedInput { resource, fd, path, device: None });
    }

    /// The granted device libinput asked to open, found by path.
    fn find_fd(&self, path: &Path) -> Option<OwnedFd> {
        let devices = self.0.borrow();
        let granted = devices.iter().find(|g| g.path.as_path() == path)?;
        granted.fd.try_clone().ok()
    }

    /// The (path, resource) pairs of granted devices libinput does not know
    /// yet.
    fn pending_devices(&self) -> Vec<(PathBuf, Resource)> {
        self.0
            .borrow()
            .iter()
            .filter(|granted| granted.device.is_none())
            .map(|granted| (granted.path.clone(), granted.resource.clone()))
            .collect()
    }

    /// Store libinput's device handle for the entry at `path` (called right
    /// after a successful `path_add_device`). Returns the device back if the
    /// entry is gone (a revoke landed between snapshot and add), so the
    /// caller can remove it from libinput instead of leaking it.
    fn attach_device(&self, path: &Path, device: input::Device) -> Option<input::Device> {
        let mut devices = self.0.borrow_mut();
        match devices.iter_mut().find(|granted| granted.path.as_path() == path) {
            Some(granted) => {
                granted.device = Some(device);
                None
            }
            None => Some(device),
        }
    }
}

/// The device-open half of [`input::LibinputInterface`] for the registry:
/// every path libinput opens is a device we granted, and the fd it gets is a
/// fresh dup of the grant.
impl input::LibinputInterface for InputRegistry {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        // libinput only ever opens what we handed it via path_add_device —
        // anything else is a bug and must fail. Never fall back to opening
        // the path ourselves (sgc-or-die).
        let fd = self.find_fd(path).ok_or_else(|| {
            eprintln!(
                "linuxsgc: input: libinput asked to open {path:?}, which was never granted — refusing"
            );
            Errno::ENOENT as i32
        })?;
        apply_open_flags(&fd, OFlag::from_bits_retain(flags))?;
        Ok(fd)
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        // Only the dup libinput opened through us. The registry keeps its
        // own dup and the SgcClient the canonical until the daemon revokes,
        // so dropping this one is all there is to do.
        drop(fd);
    }
}

/// The input state: a libinput path context plus the granted-device registry
/// behind it. Created at `Backend::build` and seeded from the session's
/// acquired inputs; device registration with libinput happens on the
/// event-loop thread ([`InputState::add_pending_devices`]).
pub struct InputState {
    /// The path-backend context over the granted devices (the registry is
    /// its open/close interface). The primary handle — clones are handed out
    /// via [`InputState::libinput`].
    libinput: input::Libinput,
    /// The granted devices this context knows about.
    registry: InputRegistry,
}

impl InputState {
    /// A new, empty path context over a fresh registry.
    pub fn new() -> Rc<Self> {
        let registry = InputRegistry::default();
        let libinput = input::Libinput::new_from_path(registry.clone());
        Rc::new(Self { libinput, registry })
    }

    /// A clone of the libinput context (refcounted — every clone is a handle
    /// to the same context). The dispatch handler and the sgc pump routing
    /// each work on their own clone.
    pub fn libinput(&self) -> input::Libinput {
        self.libinput.clone()
    }

    /// Register the input devices the session acquired at connect time.
    /// Best-effort like the acquisition itself: a device that cannot be
    /// registered is logged and skipped, never fatal.
    pub fn seed_from_session(&self, session: &SgcSession) {
        for resource in &session.inputs {
            match session.fd(resource) {
                Ok(fd) => self.registry.add_granted(resource.clone(), fd),
                Err(err) => {
                    eprintln!("linuxsgc: input: cannot dup {resource:?} for libinput: {err}")
                }
            }
        }
    }

    /// Hand every granted device without a libinput handle to libinput
    /// (path_add_device). Must run on the event-loop thread: libinput is not
    /// thread-safe and the add opens the device synchronously through the
    /// interface. Returns how many devices were added.
    pub fn add_pending_devices(&self) -> usize {
        // Snapshot the pending devices first — the add re-enters the registry
        // through the interface, so no registry borrow may be live then.
        let pending = self.registry.pending_devices();
        if pending.is_empty() {
            return 0;
        }

        let mut libinput = self.libinput.clone();
        let mut added = 0;
        for (path, resource) in pending {
            let Some(path_str) = path.to_str() else { continue };
            match libinput.path_add_device(path_str) {
                Some(device) => {
                    let name = device.name().into_owned();
                    match self.registry.attach_device(&path, device) {
                        None => {
                            added += 1;
                            println!(
                                "linuxsgc: input: libinput device added: {resource:?} at {} ({name})",
                                path.display()
                            );
                        }
                        Some(device) => {
                            // The entry vanished between the snapshot and the
                            // add (a revoke landed); do not leak the device in
                            // libinput.
                            eprintln!(
                                "linuxsgc: input: {resource:?} was revoked while being added; removing it from libinput"
                            );
                            libinput.path_remove_device(device);
                        }
                    }
                }
                None => eprintln!(
                    "linuxsgc: input: libinput rejected {resource:?} at {} — keeping the grant registered",
                    path.display()
                ),
            }
        }
        added
    }
}

/// Apply the open(2) flags libinput asked for to a dup of a granted fd.
fn apply_open_flags(fd: &OwnedFd, flags: OFlag) -> Result<(), i32> {
    // O_NONBLOCK is a file-description flag (F_SETFL): it lands on the open
    // file description shared with the daemon's fd. The access mode cannot
    // be changed with fcntl and already matches — the daemon opened the
    // device read-only and libinput only ever opens evdev devices read-only.
    if flags.contains(OFlag::O_NONBLOCK) {
        let current = nix::fcntl::fcntl(fd, FcntlArg::F_GETFL).map_err(|errno| errno as i32)?;
        let updated = OFlag::from_bits_retain(current) | OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(fd, FcntlArg::F_SETFL(updated)).map_err(|errno| errno as i32)?;
    }
    // FD_CLOEXEC is per-fd (F_SETFD): safe to set on our dup only.
    if flags.contains(OFlag::O_CLOEXEC) {
        nix::fcntl::fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
            .map_err(|errno| errno as i32)?;
    }
    Ok(())
}
