# Rendering

The backend renders one fullscreen scene on the leased DRM card. This file
covers the shared display stack, then the two renderers (software and GL),
then rotation, the mouse cursor, and what happens on preemption.

## The device opener

Every fd used for display comes from `SharedState::device_accessor()`, a
closure that:

- refuses any path whose file name is not `card{self.card}` — the backend
  will never open another DRM device, and
- returns a dup of the current lease fd from the `drm_fd` slot (error if the
  lease is revoked).

It also clears `O_NONBLOCK` on the fd (fcntl F_SETFL): DRM page-flip waits
need a blocking fd. The SCM_RIGHTS dup shares its open file description with
the daemon's copy, so this clears the flag on the shared description — the
daemon does not rely on it. Renderers call this accessor at init and rebuild
time, so a swapped (re-granted) fd is picked up by the next init.

## DrmOutput: picking the output and mode

`DrmOutput::new(device_opener)` scans `/dev/dri/*` and asks the opener for each
path; only the leased card opens, everything else is refused and skipped.
On the opened fd it:

1. reads the resource handles,
2. picks the connector: the `SLINT_DRM_OUTPUT` name if set (or `"list"` to
   print available ones), otherwise the first connected connector,
3. picks the mode: the `SLINT_DRM_MODE` index into that connector's mode list
   if set (`"list"` prints indices: resolution + refresh), otherwise the mode
   with the `PREFERRED` flag, falling back to the largest area,
4. assigns a CRTC and, at present time, commits the mode on the connector.

Presenting is one modeset/atomic commit plus a page flip; `wait_for_page_flip`
blocks (on the blocking fd) until the previous flip completes, which paces
frames to the display refresh.

## Renderer selection

Which renderer exists is a cargo feature; which one runs is decided at window
creation:

- only `renderer-software` enabled → software renderer
- only `renderer-femtovg` enabled → GL renderer
- both enabled and no explicit choice → femtovg first, fall back to software
  if its initialization fails
- explicit name via the backend builder (from `SLINT_BACKEND=linuxsgc-femtovg`
  or `linuxsgc-software`)

## Software renderer

The software path uses **dumb buffers**: the display owns N dumb framebuffers
in the supported formats (XRGB8888, ARGB8888, BGRA8888, RGB565); the
negotiated format is the first the renderer and the display both support.

Frame pipeline:

flowchart TD
    RIF["render_if_needed<br/>(redraw requested, not suspended)"] --> MAP["map_back_buffer: lock the next buffer"]
    MAP --> ROT["SoftwareRenderer.set_rendering_rotation(rotation)"]
    ROT --> RENDER["renderer.render(buffer, stride)<br/>scene drawn into the typed pixel slice"]
    RENDER --> CUR{"pointer exists?"}
    CUR -->|yes| BLIT["composite cursor bitmap over the frame<br/>(premultiplied alpha blend, per-pixel)"]
    CUR -->|no| PRES
    BLIT --> PRES["present (page flip)"]
    PRES --> FLIP["wait_for_page_flip on the next frame"]

- `render()` paints the whole scene every frame; buffer age selects the
  repaint-buffer strategy (`NewBuffer` first frame, then reused/swapped).
- The **cursor is composited after the scene render** directly into the frame
  buffer: the cached cursor pixels (an embedded SVG rasterized to a
  premultiplied RGBA image once) are blended per pixel over the rendered
  scene at the pointer position. This is renderer-side; the scene knows
  nothing about it.
- Rotation is handled twice: the renderer rotates the scene
  (`set_rendering_rotation`), and the cursor blit maps window coordinates to
  buffer coordinates with the same mirror/transpose transform the renderer
  uses, so the cursor tracks correctly under any rotation.

### Rebuild (re-grant survival)

The software renderer is fd-independent: only the display stack (dumb
buffers, framebuffers, CRTC state) is bound to the lease fd. `rebuild()`
re-runs display init on the new fd and keeps the existing `SoftwareRenderer`.
Preemption costs one frozen frame and a rebuild — the process survives.

## GL renderer (femtovg)

The GL path uses a **gbm display** on the lease fd, an EGL display and a
GLES2 (fallback: any) context, and a gbm-backed window surface:

flowchart TD
    NEW["FemtoVGRendererAdapter::new(accessor)"] --> OUT["DrmOutput::new(accessor)<br/>connector + mode (same selection as sw)"]
    OUT --> GBM["GbmDisplay::new: gbm device on the lease fd<br/>gbm surface + EGL window"]
    GBM --> GLX["EGL display, config (no transparency,<br/>lowest samples), GLES2 context, window surface"]
    GLX --> RIF["render_and_present per frame"]
    RIF --> RCTX["render_transformed_with_post_callback<br/>(rotation degrees + translation, size)"]
    RCTX --> CB["post-render callback: draws the cursor<br/>in logical space under the scene transform"]
    CB --> PRES2["present (gbm buffer swap + page flip)"]
    PRES2 --> WAIT["wait_for_page_flip first (previous frame posted)"]

The scene is rendered rotated by the GL transform (`rotation.degrees()` +
`translation_after_rotation(size)`); the cursor is drawn through the same
transform by the post-render callback (an `ItemRenderer` pass after the
scene), so it needs no manual coordinate mapping on this path.

### Rebuild (preemption)

The GL renderer **cannot be rebuilt in-process**: its EGL/GL context is bound
to the lease fd and dies with it. `rebuild()` is not implemented, so a
revoke→re-grant cycle fails at the rebuild step, the error is treated as
fatal, and the event loop (and process) exits with an error. This is a
documented limitation: on this backend, a GL app is not preemption-surviving
— run it only where the lease is not contended, or use the software renderer
when preemption survival matters.

## Rotation

`SLINT_KMS_ROTATION` (0/90/180/270) rotates the rendered scene. The window's
logical size is the screen size rotated back (`screen_size_to_rotated_window_size`),
so the slint scene lays out for the rotated viewport; the renderer presents
the rotated result to the physical screen. The software renderer maps every
scene coordinate (and the cursor) through mirror + transpose; the GL renderer
uses its rotation/translation transform.

## Mouse cursor

- The cursor is the embedded `mouse-pointer.svg`, rasterized once to a
  premultiplied RGBA image and cached.
- Its position is the backend's `mouse_position` property, updated by the
  input handler on pointer motion (see input.md).
- Software renderer: pixel-composited after each scene render at the physical
  pointer position.
- GL renderer: drawn by the post-render callback in the transformed scene
  space.
- The area under the cursor is marked dirty so partial repaints erase the
  previous cursor position.

Without a pointer (no libinput feature / no mouse grant) the property stays
`None` and no cursor is drawn.
