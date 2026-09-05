# linuxsgc backend — design documentation

The `i-slint-backend-linuxsgc` crate is a Slint backend for Linux that renders
one fullscreen window on a **DRM lease granted by the simple-graphics-controller
daemon** (@sgc). The backend owns the whole @sgc session — connecting to the
daemon, acquiring the resources, and surviving revoke/re-grant — and the
application never touches the daemon, DRM, or input devices itself.

This directory documents the backend **as it currently works** (branch
`sgc-lease-1.17`). Every diagram is the current design; there is no
alternatives history here.

## The model in one paragraph

An app enables the `backend-linuxsgc` slint feature (plus a renderer). When
the first window is created, the backend selector instantiates this backend,
which:

1. connects to the daemon's abstract socket `@sgc` and acquires the first
   advertised `Drm` card (a lease — never DRM master; **sgc or die**: no
   daemon, no card, no fallback),
2. acquires every advertised input device (keyboard/mouse/touch) — best
   effort, only when the `libinput` feature is compiled in,
3. opens the leased card through a *device opener* that refuses everything
   except that one card, builds a display stack on it, and renders the slint
   scene fullscreen,
4. pumps the @sgc session on a 50 ms timer: a **revoke** suspends rendering,
   a **re-grant** rebuilds the display stack on the fresh fd,
5. feeds granted input device fds into **libinput** (path backend) and
   dispatches pointer/keyboard/touch events to the window.

The rest of this directory explains each piece:

| File | What it documents |
| --- | --- |
| architecture.md | Modules, ownership, startup and event-loop anatomy, sgc event routing |
| rendering.md | Display stack: mode selection, software renderer, GL renderer, cursor, preemption behavior |
| input.md | Daemon-granted input devices through libinput: registration, dispatch, live revoke/regrant |
| app-guide.md | How to build and run an application on this backend (features, env vars, board deployment) |

## Component map

graph TD
    APP["slint app<br/>(enables slint features)"] --> SEL["backend selector<br/>SLINT_BACKEND=linuxsgc"]
    SEL --> BE["Backend<br/>(calloop_backend/mod.rs)"]
    BE --> SESS["SgcSession<br/>(sgc.rs)"]
    SESS --> DAEMON["@sgc daemon<br/>(separate process, owns /dev/dri + /dev/input)"]
    BE --> ADAPTER["FullscreenWindowAdapter"]
    ADAPTER --> REN["FullscreenRenderer"]
    REN --> SW["SoftwareRendererAdapter<br/>(dumb buffers)"]
    REN --> GL["FemtoVGRendererAdapter<br/>(gbm/EGL)"]
    REN --> OUT["DrmOutput + display stack<br/>(drmoutput.rs)"]
    BE --> INPUT["InputState + registry<br/>(calloop_backend/input_shared.rs)"]
    INPUT --> LI["LibInputHandler<br/>(calloop_backend/input.rs)"]
    LI --> ADAPTER
    SESS -->|"granted fds (SCM_RIGHTS)"| INPUT
    SESS -->|"lease fd dup"| OUT

## Quick start

See app-guide.md for the full recipe. Minimal app wiring:

```toml
# Cargo.toml
slint = { git = "https://github.com/lulkien/slint.git", branch = "sgc-lease-1.17",
          default-features = false,
          features = ["std", "compat-1-2", "backend-linuxsgc",
                      "renderer-software"] }
```

```rust
// main.rs — nothing backend-specific; creating the window starts the backend.
slint::slint! { export component MainWindow inherits Window { /* ... */ } }
fn main() {
    let ui = MainWindow::new().expect("start the @sgc daemon first");
    ui.run().unwrap();
}
```

Run with the daemon up; the window is fullscreen on the leased card. See
app-guide.md for renderer and input options.
