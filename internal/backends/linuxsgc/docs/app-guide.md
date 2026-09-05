# Application guide

How to build and run a Slint app on this backend, from Cargo wiring to the
board.

## Requirements

- A running simple-graphics-controller daemon (@sgc, abstract socket) that
  advertises a DRM card — the backend is **sgc or die**: no daemon, no card,
  or a denied acquire = startup error.
- Linux. The demo flavors deploy to an aarch64 board running glibc (gnu
  builds); see "Board builds" below.
- Fonts: text rendering goes through fontique with fontconfig dlopened at
  runtime. Either have fontconfig data on the target (fonts + `/etc/fonts`),
  or register font files into the shared collection yourself.

## Cargo wiring

The app never names the backend crate or the @sgc client library. It enables
slint facade features; the selector auto-installs the backend on first window
creation (when linuxsgc is the only backend enabled, no `SLINT_BACKEND`
variable is needed).

```toml
# Cargo.toml
slint = { git = "https://github.com/lulkien/slint.git", branch = "sgc-lease-1.17",
          default-features = false,
          features = ["std",
                      "compat-1-2",                 # mandatory compat feature
                      "backend-linuxsgc",           # the backend itself
                      "renderer-software"] }        # or renderer-femtovg
```

| feature | effect |
| --- | --- |
| backend-linuxsgc | the backend (implies `std`); select with `SLINT_BACKEND=linuxsgc` |
| backend-linuxsgc-libinput | input from the daemon-granted devices via libinput (pointer/keyboard/touch) — add to the list above |
| renderer-software | CPU rendering (dumb buffers); the only choice for fully static musl builds |
| renderer-femtovg | OpenGL over gbm/EGL on the lease (Mali/panfrost, etc.); gnu/dynamic builds |

```rust
// main.rs — nothing backend-specific.
slint::slint! {
    export component MainWindow inherits Window {
        // ... your UI ...
    }
}
fn main() -> Result<(), slint::PlatformError> {
    // Creating the window starts the backend: it connects to @sgc and
    // acquires the lease. Error message says so if the daemon is missing.
    let ui = MainWindow::new()?;
    ui.run()?;
    Ok(())
}
```

Renderer selection when both features are enabled:
`SLINT_BACKEND=linuxsgc-femtovg` or `SLINT_BACKEND=linuxsgc-software`
(default: femtovg, falling back to software if GL init fails).

## Environment variables

| variable | meaning |
| --- | --- |
| SLINT_BACKEND | backend selection: `linuxsgc`, `linuxsgc-software`, `linuxsgc-femtovg` |
| SLINT_DRM_OUTPUT | connector by name, or `list` to print them |
| SLINT_DRM_MODE | mode index into the connector's mode list, or `list`; default = PREFERRED, else largest |
| SLINT_KMS_ROTATION | 0/90/180/270 — rotate the rendered scene |
| SLINT_SCALE_FACTOR | window scale factor (applied at window creation) |

Daemon side: `SGC_POLICY=first-owner|latest-owner|fair-queue` (default
fair-queue) governs what happens when a second client acquires a held
resource — all resources (Drm, Fbdev, Input) follow the policy.

## Builds

### Current host (check / quick run)

```sh
cargo build -p my-app --features "backend-linuxsgc,renderer-software,backend-linuxsgc-libinput"  # host: needs libinput + libxkbcommon dev
```

### Board (aarch64, gnu, dynamic — the standard deployment)

```sh
PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
cargo build --release --target aarch64-unknown-linux-gnu \
    --features "backend-linuxsgc,renderer-software,backend-linuxsgc-libinput"
```

- The `backend-linuxsgc-libinput` (and `renderer-femtovg`) flavors link
  system libraries — do NOT set `PKG_CONFIG_ALL_STATIC=1` for them (libinput
  must stay dynamic). Static (musl) builds: software renderer, no input
  feature.
- Host cross prerequisites: arm64 pkg-config files for freetype (fontique),
  and for input builds also `libinput.pc` + `libxkbcommon.pc`
  (packages `libinput-dev:arm64`, `libxkbcommon-dev:arm64`).

### Runtime libraries on the board

| flavor | board packages |
| --- | --- |
| software (no input) | glibc; fonts (fontconfig data) |
| software + input | + `libinput10`, `libxkbcommon0` |
| femtovg (GL) | + mesa: `libgbm1`, `libegl1`, `libgles2` (+ the kernel's GPU driver, e.g. panfrost) |

Install missing ones with apt on the board and record them — the convention
is dynamic-first, never switch to static to dodge device libraries.

## Deploying and running on the board

1. scp the binary to the board (e.g. `/root/my-app-gnu`).
2. Start the daemon if not running (it opens /dev/dri + /dev/input as root):
   `nohup /root/simple-graphics-controller-gnu > /tmp/sgc-daemon.log 2>&1 &`
3. Start the app: `nohup /root/my-app-gnu > /tmp/app.log 2>&1 &`

Expected log lines (both go to stdout):

- daemon: `Opened /dev/dri/card0 (Drm { card: 0 }) ...`, one
  `Opened /dev/input/eventN (name): Input(...)` per device,
  `Listening on abstract socket @sgc`
- app: `connected to @sgc; available: [...]`,
  `linuxsgc: acquiring Drm{ card: 0 } ...`,
  `linuxsgc: lease for Drm { card: 0 } granted`,
  per input: `acquiring ... granted`, `registered granted ...:
  /dev/input/eventN`, and — once the loop runs —
  `linuxsgc: input: libinput device added: Input(...) at /dev/input/eventN (name)`,
  `Using Software renderer` / `Using FemtoVG OpenGL renderer`,
  `Rendering at WxH`.

Operational notes learned on the board:

- The daemon enumerates input devices at startup only — a keyboard/mouse
  plugged later needs a daemon restart to be advertised.
- Killing the app (or any DRM client) leaves the screen frozen on the last
  frame; recover with the daemon running and restarting the app (chvt 7 &&
  chvt 1 if the console looks stuck).
- The app must be started with the daemon already running.

## Behavior contract

What the app can rely on:

- Fullscreen single window on the leased output; no window management, no
  multiple outputs.
- Renderer choice: feature-gated (see above). Software survives preemption;
  **femtovg exits with an error when preempted** (its EGL/GL context cannot
  be rebuilt in-process) — use software where lease contention is possible.
- Input arrives only when the `backend-linuxsgc-libinput` feature is on and
  devices were granted; failures are non-fatal, a keyboard-less run is fine.
- Preemption follows the daemon's policy for every resource kind: the app is
  revoked, suspends (DRM) / drops the device (input), and is re-granted when
  the lease frees up.
- A daemon death is fatal: the session cannot outlive the daemon; the event
  loop ends with an error.
- Keyboard: Ctrl+Alt+Backspace (or Delete) quits the loop — built in.

## Known limitations (current state)

- One window, fullscreen, no cursor-driven compositing beyond the backend's
  own pointer cursor.
- The first advertised DRM card is acquired; if its acquire is denied the
  backend fails rather than trying another advertised card.
- No input hot-plug (daemon-side limitation).
- Input + GL flavors are dynamically-linked-gnu only; musl static = software
  renderer, no input.
- libinput's dispatch must keep up; on very slow boards libinput may log
  "client bug: event processing lagging behind" when startup init stalls the
  loop briefly — it settles once the loop runs.

## Examples in this fork

- `sgc-demos/slint-lease-client` — minimal: a bouncing square on the lease,
  input via `--features input`. Reference for Cargo wiring and the Justfile
  board recipes.
- `demos/energy-monitor` (slint repo) — dashboard; its `sgc` (software) and
  `sgc-femtovg` (GL) features map to this backend. Board run:
  `--no-default-features --features "sgc-femtovg,chrono"` (GL) and, when the
  display mode needs pinning, `SLINT_DRM_MODE=<index>`.
