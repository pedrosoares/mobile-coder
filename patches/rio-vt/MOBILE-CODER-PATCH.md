# Local patch: rio-vt 0.5.26 on Android

Vendored and wired in with `[patch.crates-io]`. The kitty graphics protocol's
shared-memory transmission medium calls `libc::shm_open`/`shm_unlink`, which
bionic does not provide, so the crate did not compile for Android.

On Android that medium now returns `GraphicError::FileNotFound`, as it already
does when a shared-memory object is missing. A program inside the proot guest
could not share memory with the app process in any case. Other transmission
media (direct, file, temp file) are untouched. Change marked `mobile-coder
patch` in `src/ansi/kitty_graphics_protocol.rs`.
