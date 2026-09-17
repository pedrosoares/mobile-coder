# The execution probe

`tools/exec-probe` answers one question on real hardware:

> With `targetSdkVersion >= 29`, can an unrooted Android app run a Linux userland
> under proot?

Everything about the architecture follows from the answer, and it is not a
question worth settling by reasoning. This measures it.

## Why it has to be an app

The probe **must run inside the app process.**

`adb shell` runs as the `shell` user, in a different SELinux domain, where
`/data/local/tmp` *is* executable. A shell-based test passes and tells you
nothing about `untrusted_app`. The same applies to anything run through
`run-as`. Hence a real APK, a real Activity, and a JNI call into Rust.

## What it checks

Run in order; each one is reported separately.

| Check | Question | Expected |
|---|---|---|
| `direct-bionic` | Direct `exec()` of a bionic binary copied into app data | **BLOCKED** — the control |
| `linker-bionic` | Same binary via `/system/bin/linker64` | WORKED — this is what `termux-exec` does |
| `linker-musl` | An **Alpine (musl)** binary via bionic's linker | **unknown — the question** |
| `nativelib-exec` | proot straight from the native library directory | WORKED — `jniLibs` is always executable |
| `proot-guest` | proot running a guest binary end to end | **THE ANSWER** |
| `devptmx` | `/dev/ptmx` opens read-write | WORKED — `mc-pty` needs it |

### `direct-bionic` is the control, and it is meant to fail

If direct exec from app data *succeeds*, the restriction is not in force — either
`targetSdk` got lowered somewhere, or SELinux is permissive on that device. Every
later result is then measuring the wrong system. The probe says so in its verdict
rather than letting you read the rest as good news.

### Why `linker-musl` and `proot-guest` are separate

`linker-musl` isolates the loader question: can bionic's `linker64` load a
musl-linked ELF at all? `proot-guest` is the end-to-end one, and it is the result
that actually decides the architecture — proot performs guest `execve` calls
itself, deep inside its own ptrace loop, so it is entirely possible for one of
these to pass and the other to fail. Reporting them separately is what makes the
failure mode legible instead of just "it didn't work".

## Running it

### Prerequisites

```sh
cargo install cargo-ndk
rustup target add aarch64-linux-android
export ANDROID_HOME=<sdk>          # API 36
export ANDROID_NDK_HOME=<ndk>      # r26d
```

Plus a device or emulator on `adb`, with **SELinux enforcing**. Confirm with
`adb shell getenforce` — a `Permissive` result invalidates the run.

> **Emulator: use `-gpu host`.** The default (`swiftshader_indirect`) segfaults
> during boot on Fedora 44 / kernel 7.1.13 / Mesa 26.1.8 — the *guest* kernel boots
> fine and the host `qemu-system-x86_64-headless` process dies. The backtrace puts
> it inside SwiftShader's JIT (`emulator/lib64/gles_swiftshader/libGLESv2.so`).
> `-gpu host` and `-gpu angle_indirect` both work; `-gpu guest` does not, because it
> silently falls back to SwiftShader. The system image is irrelevant — `default` and
> `aosp_atd` fail identically. Known-good invocation:
>
> ```sh
> emulator -avd <name> -no-window -no-audio -no-snapshot -gpu host -no-boot-anim
> ```

### Fetch the third-party binaries

```sh
tools/exec-probe/fetch-assets.sh
```

This downloads, and records in `assets/MANIFEST.txt`:

- the latest **Alpine aarch64 minirootfs** — the musl guest binaries to test against;
- **Termux's prebuilt proot**, unpacked from its `.deb`, so we do not need an NDK
  build of proot just to answer a yes/no question.

proot is not self-contained: it execs a small loader ELF whose path it reads from
`PROOT_LOADER`. Both binaries are installed into `jniLibs/arm64-v8a/` as
`libproot.so` and `libproot-loader.so` — Android only extracts files matching
`lib*.so`, and the native library directory is the one place an app may execute
from at any `targetSdkVersion`.

### Build, install, run

```sh
tools/exec-probe/run-probe.sh
```

It builds the cdylib with `cargo-ndk`, assembles the APK, installs, launches, and
prints the report. Results also land on-screen, in `logcat -s mc-exec-probe`, and
as JSON at `files/probe-report.json` inside the app sandbox.

> The Gradle project has no committed wrapper. Run `gradle wrapper` once in
> `tools/exec-probe/AndroidApp/`, or open it in Android Studio.

## Reading the verdict

The probe ends with one of three conclusions:

- **CONTROL FAILED** — `direct-bionic` was not blocked. Fix that first; nothing
  else in the report means anything yet.
- **PROOT WORKS** — a Linux userland is viable at this `targetSdk`. Proceed, and
  Google Play stays available.
- **PROOT CANNOT RUN GUEST BINARIES** — the options are `targetSdk 28` (sideload
  only), or replacing the Linux rootfs with a bionic-native toolchain. See
  `ARCHITECTURE.md` §2.2.

## Recording the result

Paste the JSON report into this file under a dated heading when you run it. The
whole point is to replace an assumption with a measurement, and a measurement
nobody wrote down decays back into an assumption.

### Results

#### 2026-09-16 — Samsung Galaxy Z Fold6 (SM-F956B), Android 16, arm64

The physical-device confirmation the emulator run called for, on an OEM build
with Samsung's own SELinux policy layered over AOSP. Conditions: `targetSdk=36`,
SELinux **Enforcing**, context `u:r:untrusted_app:s0:c16,c258,c512,c768`,
kernel `6.1.128-android14`.

| Check | Outcome |
|---|---|
| `direct-bionic` | **BLOCKED** — `EACCES` (control holds) |
| `linker-bionic` | **WORKED** — `stdout="alive"` |
| `linker-musl` | **BLOCKED** — killed by a signal (no exit code) |
| `nativelib-exec` | **WORKED** |
| `proot-guest` | **WORKED** — `stdout="alive-inside-proot"` |
| `mc-sandbox-e2e` | **WORKED** — `stdout="hi-from-mc-sandbox"` |
| `devptmx` | **WORKED** |

**Verdict: identical to the emulator.** proot runs a Linux userland at
`targetSdk 36` on real arm64 hardware under an OEM SELinux policy. The emulator
caveat below is retired for Samsung.

`linker-musl` fails differently here - killed by a signal rather than the x86_64
*COPY relocations are not supported* error - but it is a negative either way, and
not the route proot uses.

#### 2026-09-15 — Android emulator, API 36, x86_64

Conditions: `targetSdk=36`, SELinux **Enforcing**, context
`u:r:untrusted_app:s0:c150,c256,c512,c768`. Control held.

| Check | Outcome |
|---|---|
| `direct-bionic` | **BLOCKED** — `EACCES` (control holds: the restriction is in force) |
| `linker-bionic` | **WORKED** — `stdout="alive"` |
| `linker-musl` | **INCONCLUSIVE** — `CANNOT LINK EXECUTABLE … COPY relocations are not supported` |
| `nativelib-exec` | **WORKED** — proot 5.1.107.92 ran from `jniLibs` |
| `proot-guest` | **WORKED** — `stdout="alive-inside-proot"` |
| `mc-sandbox-e2e` | **WORKED** — `stdout="hi-from-mc-sandbox"` |
| `devptmx` | **WORKED** |

`mc-sandbox-e2e` drives `mc_sandbox::Sandbox::run()` through the real
`ProotBackend` — the same code path the app uses. The probe depends on
`mc-sandbox` rather than carrying its own copy, so a regression in shipped code
shows up here.

**Verdict: proot works at `targetSdk 36`.** A Linux userland is viable without
dropping to `targetSdk 28`, so Google Play distribution stays open.

**Why this works, given `direct-bionic` is blocked.** The two are not in conflict,
and the distinction is the whole finding. SELinux denies `execve` on files under
app data — that is what `direct-bionic` measures. proot never asks the kernel to
`execve` a guest binary: it ptraces the tracee and hands control to its own small
loader ELF, which ships in the native library directory (always executable) and
**maps** the guest binary itself. Mapping executable pages from app data is
evidently still permitted; only `execve` is denied. So proot sidesteps the rule
rather than defeating it.

**`linker-musl` is a genuine negative, and it no longer matters.** bionic's linker
did find musl's libc once pointed at the rootfs — it got far enough to fail on
*COPY relocations are not supported*, a real ABI-level incompatibility. So the
`system_linker_exec` route really is closed to a musl userland. It is simply not
the route proot takes, which is why the end-to-end check passes anyway.

**Building software on the phone works.** Confirmed on the Fold6 on 2026-09-16:
the agent wrote a C file, then ran `gcc -O2 -Wall -o hello hello.c && ./hello`
inside the guest and got the expected output. That exercises the full
`gcc` -> `cc1` -> `as` -> `ld` fork/exec chain through proot. Three platform
problems had to be fixed to get there, each found by a real run:

- **Hard links are denied.** `link()` on app data fails with `EACCES` (SELinux,
  not seccomp - it fails under `run-as` too). Alpine's `gcc` package ships
  `/usr/bin/gcc` as a hard link, so `apk add build-base` installed the target
  but not `gcc` itself. Fixed with proot's `--link2symlink`.
- **The guest has no DNS.** Android has no `/etc/resolv.conf` and the minirootfs
  ships none, so `apk update` failed with *"temporary error"*. Fixed by writing
  the phone's own per-network DNS servers from `ConnectivityManager`, rewritten
  on every network change.
- **proot's temp directory pointed at Termux's install**, spraying warnings on
  every command. Fixed with `PROOT_TMP_DIR`.

A connectivity caveat for LAN endpoints such as LM Studio: this phone has
Samsung's *switch to mobile data* enabled, and twice a long `apk` download was
followed by `error sending request` to the LAN address, which does not exist on
cellular. Not proven to be a network switch at that exact moment - idle time
alone (10s, 90s) did not reproduce it - but connect and send failures are now
retried with backoff. For LAN development, turning that setting off removes the
variable.

**Known x86_64-only limitation: musl cannot `fork` inside the app.** Found on
2026-09-16 when a real agent turn ran `cat /etc/alpine-release && uname -m`:
`/bin/sh: can't fork: Function not implemented` (exit 2). A single command works
because the shell `exec`s it without forking; anything compound (`&&`, pipes,
subshells) forks and fails.

Cause, confirmed on both ends:

- Zygote installs a seccomp filter in app processes (`Seccomp: 2` in
  `/proc/<pid>/status`). Android's app allowlist permits `fork()` only for
  `lp32` - on 64-bit ABIs the raw `fork` syscall is blocked, returning `ENOSYS`.
  bionic never notices, because it forks via `clone`.
- musl's `_Fork()` calls `SYS_fork` whenever the architecture defines it, and
  `SYS_clone` otherwise. x86_64 defines `__NR_fork` (57); **aarch64 does not**,
  so musl on arm64 already uses `clone`.

So this breaks on the x86_64 emulator and should not occur on arm64 phones - the
real target. That conclusion rests on the musl source and Android's policy file,
not yet on an arm64 run. glibc forks via `clone` on every architecture, so a
Debian/Ubuntu rootfs would not hit this even on x86_64.

It also explains a misleading repro: `run-as` processes have no seccomp filter
(`Seccomp: 0`), so the identical proot invocation succeeded 10/10 there while
failing 3/3 inside the app. Reproduce app-domain behaviour through the app, not
through `run-as`.

**Caveat.** This was measured on an emulator (x86_64, AOSP image). The mechanism
is architecture-independent, but OEM SELinux policies vary — re-run on a physical
arm64 device before treating it as settled for shipping.

**Two probe bugs this run caught**, both of which had produced a confidently wrong
verdict on the first attempt:

- proot is not statically linked. Without `libtalloc` and `libandroid-shmem` it
  died with `CANNOT LINK EXECUTABLE`, which the probe scored as BLOCKED and
  reported as "proot cannot run guest binaries" — a packaging mistake dressed up
  as an architectural finding. There is now an `INCONCLUSIVE` outcome for
  dynamic-linking failures, and the verdict refuses to draw a conclusion from one.
- The native library directory is not on the default search path for a spawned
  process, so `LD_LIBRARY_PATH` must point at it.
