//! First-run setup: get a Linux userland onto the device and prove it runs.
//!
//! Runs on a background thread. Downloading and unpacking a rootfs takes tens of
//! seconds on a phone, and doing it on the UI thread would mean the app's first
//! impression is a frozen window.

use std::path::PathBuf;

use mc_sandbox::{ProotBackend, Progress, RootfsSpec, Sandbox};

/// Name of our own `cdylib`, used to locate the native library directory.
/// Must match `[lib] name` in `Cargo.toml`.
const SELF_LIB: &str = "libmobile_coder.so";

/// Install the rootfs if needed, then run a command in it to prove the whole
/// chain works on this device.
///
/// Logs rather than returns: this is startup diagnostics, and on a phone the
/// only reliable channel for it is logcat.
pub fn run(files_dir: PathBuf) {
    let Some(lib_dir) = mc_sandbox::android::native_lib_dir(SELF_LIB) else {
        log::error!("could not locate the native library directory; sandbox unavailable");
        return;
    };
    log::info!("native library directory: {}", lib_dir.display());

    let spec = RootfsSpec::alpine(&files_dir);
    log::info!("rootfs: {} -> {}", spec.url, spec.dest.display());

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error!("no tokio runtime: {e}");
            return;
        }
    };

    let installed = runtime.block_on(mc_sandbox::rootfs::ensure(&spec, &|progress| {
        match progress {
            Progress::AlreadyReady => log::info!("rootfs already installed"),
            // Only log at boundaries - a per-chunk log would flood logcat and
            // slow the download it is reporting on.
            Progress::Downloading { downloaded, total } => match total {
                Some(total) if downloaded == total => {
                    log::info!("rootfs downloaded ({downloaded} bytes)")
                }
                _ => {}
            },
            Progress::Verifying => log::info!("verifying checksum"),
            Progress::Extracting { entries } => log::info!("extracted {entries} entries"),
            Progress::Ready => log::info!("rootfs ready"),
        }
    }));

    let rootfs = match installed {
        Ok(path) => path,
        Err(e) => {
            log::error!("rootfs install failed: {e}");
            return;
        }
    };

    let sandbox = Sandbox::new(Box::new(
        ProotBackend::from_native_lib_dir(&lib_dir, &rootfs).with_tmp_dir(files_dir.join("proot-tmp")),
    ));

    // Publish readiness only after a guest command has actually succeeded -
    // "the files are unpacked" is not the same as "the sandbox works".
    let mut guest_ok = false;

    for command in ["uname -a", "cat /etc/alpine-release", "id"] {
        match runtime.block_on(sandbox.run(command, None)) {
            Ok(out) if out.ok() => {
                guest_ok = true;
                log::info!("[guest] {command} => {}", out.stdout.trim());
            }
            Ok(out) => log::error!(
                "[guest] {command} failed ({:?}): {}",
                out.status,
                out.stderr.trim()
            ),
            Err(e) => log::error!("[guest] {command} could not run: {e}"),
        }
    }

    if guest_ok {
        crate::agent_task::mark_ready(lib_dir, rootfs);
        log::info!("sandbox ready; the agent can run");
    } else {
        log::error!("no guest command succeeded; the agent will stay disabled");
    }
}
