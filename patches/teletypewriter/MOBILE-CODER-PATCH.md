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

One more, not a compile error but a hang: the shell inherited the launcher's
*blocked* signals. `default_shell_command` already reset the ignored
dispositions (Rust ignores SIGPIPE for the whole process), but an Android app
runs its threads with signals blocked too - SIGPIPE among them - and a blocked
signal survives `exec` just as an ignored one does. A pipeline like
`yes | head -3` then never ends, because the write returns `EPIPE` instead of
killing the writer and busybox loops on it. The mask is now cleared before
exec. Measured on a Galaxy Z Fold6; see `docs/PHONE-TESTS.md`, check 3.

Changes are marked `mobile-coder patch` in `src/unix/mod.rs`. Worth sending
upstream; drop this directory once a fixed release exists.
