
**NOTE**: This library is an **internal** crate of the [Slint project](https://slint.dev).
This crate should **not be used directly** by applications using Slint.
You should use the `slint` crate instead.

**WARNING**: This crate does not follow the semver convention for versioning and can
only be used with `version = "=x.y.z"` in Cargo.toml.

## Design documentation

The current design of this backend (session, event loop, rendering, input,
application guide) is documented under [`docs/`](docs/) — see
[`docs/README.md`](docs/README.md) for the map. Apps do not use this crate
directly: enable the `backend-linuxsgc` (+ optional `backend-linuxsgc-libinput`,
plus a renderer) features on the `slint` crate instead.
