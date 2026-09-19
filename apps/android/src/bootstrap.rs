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

    // Set when this launch actually installed the rootfs, so the one-time
    // package setup below runs on a fresh guest and not on every start.
    let fresh = std::sync::atomic::AtomicBool::new(true);

    let installed = runtime.block_on(mc_sandbox::rootfs::ensure(&spec, &|progress| {
        match progress {
            Progress::AlreadyReady => {
                fresh.store(false, std::sync::atomic::Ordering::Relaxed);
                log::info!("rootfs already installed")
            }
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

    // git is not in the minirootfs, and both the agent's local git tools and
    // any cloned project need it. Best effort: a failure here (no network on
    // first launch, say) leaves a working sandbox without git, and `apk add
    // git` from the terminal fixes it later.
    //
    // DNS first. `mark_ready` writes the guest's resolv.conf, and it runs after
    // this block - without this call a fresh rootfs has no nameserver, and apk
    // would fail to resolve the mirror.
    crate::network::apply(&rootfs);

    if guest_ok {
        // Every launch, not only a fresh rootfs: a guest installed before this
        // was known has a broken git until it is set, and the user's own
        // commands in the Terminal need it as much as the app's do.
        //
        // Android forbids hard links, so proot emulates them - and the
        // emulation puts a loose object somewhere git cannot find it again,
        // leaving `refs/heads/main` pointing at a commit that does not exist.
        // `rename` is git's own answer for filesystems like this one.
        match runtime.block_on(
            sandbox.run("git config --global core.createObject rename", None),
        ) {
            Ok(out) if out.ok() => log::info!("git configured for a filesystem without hard links"),
            Ok(out) => log::warn!("could not configure git: {}", out.stderr.trim()),
            Err(e) => log::warn!("could not configure git: {e}"),
        }
    }

    if guest_ok && fresh.load(std::sync::atomic::Ordering::Relaxed) {
        log::info!("installing git into the new guest");
        match runtime.block_on(sandbox.run("apk add --no-progress git", None)) {
            Ok(out) if out.ok() => log::info!("git installed"),
            Ok(out) => log::warn!("could not install git: {}", out.stderr.trim()),
            Err(e) => log::warn!("could not install git: {e}"),
        }
    }

    if guest_ok {
        crate::agent_task::mark_ready(lib_dir, rootfs);
        log::info!("sandbox ready; the agent can run");
    } else {
        log::error!("no guest command succeeded; the agent will stay disabled");
    }
}
