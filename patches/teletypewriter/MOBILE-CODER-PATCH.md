# Local patch: teletypewriter 0.5.27 on Android

Vendored from crates.io and wired in with `[patch.crates-io]` in the workspace
`Cargo.toml`. `freya-terminal` -> `rio-vt` -> `teletypewriter` did not compile
for `target_os = "android"`:

- `create_termp()` declares its `termios` only for `linux`, `macos` and
  `freebsd`. bionic's `termios` is the kernel layout *without* glibc's trailing
  `c_ispeed`/`c_ospeed` (and without musl's `__c_ispeed`), so no existing arm
  fits. Added an Android arm.
- `TIOCSWINSZ` had no Android definition. Added one (`c_int`, as bionic's
  `ioctl` expects).
- `openpty`/`forkpty` were declared with `#[link(name = "util")]` everywhere.
  bionic has them in libc and ships no libutil, so linking failed. The attribute
  is now skipped on Android.

Changes are marked `mobile-coder patch` in `src/unix/mod.rs`. Worth sending
upstream; drop this directory once a fixed release exists.
