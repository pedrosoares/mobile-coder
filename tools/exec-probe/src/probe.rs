//! Does an Android app with `targetSdkVersion >= 29` get to run a Linux userland?
//!
//! This exists because the answer decides the architecture, and because reasoning
//! about SELinux from a desk is how people end up confidently wrong. It measures
//! rather than argues.
//!
//! # Why this has to be an app
//!
//! It must run **inside the app process**. `adb shell` runs as the `shell` user in
//! a different SELinux domain, where `/data/local/tmp` is executable - a test run
//! there passes and proves nothing about `untrusted_app`.
//!
//! # What it establishes, in order
//!
//! | Check | Question |
//! |-------|----------|
//! | `selinux`        | Is SELinux enforcing, and what domain are we in? |
//! | `direct-bionic`  | Is direct `exec()` from app data actually blocked? |
//! | `linker-bionic`  | Does `system_linker_exec` work for a *bionic* binary? |
//! | `linker-musl`    | Does it work for an **Alpine/musl** binary? |
//! | `nativelib-exec` | Is the native library directory executable? |
//! | `proot-guest`    | **The decisive one.** Can proot run a guest binary end to end? |
//! | `devptmx`        | Is `/dev/ptmx` usable, for `mc-pty`? |
//!
//! `direct-bionic` is expected to FAIL on `targetSdk >= 29`. That failure is the
//! control: if it unexpectedly succeeds, the restriction is not in force and every
//! later result is measuring the wrong thing.

use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use mc_sandbox::{ProotBackend, Sandbox};
use serde_json::{Value, json};

/// Bionic's loader for 64-bit processes.
const LINKER64: &str = "/system/bin/linker64";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The thing worked.
    Worked,
    /// The thing failed.
    Blocked,
    /// Could not be run (missing input, wrong arch).
    Skipped,
    /// It failed, but for a reason that is not the thing being measured -
    /// typically a missing shared library. Kept distinct from [`Self::Blocked`]
    /// because conflating them turns a packaging mistake into a false
    /// architectural conclusion.
    Inconclusive,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Worked => "WORKED",
            Self::Blocked => "BLOCKED",
            Self::Skipped => "SKIPPED",
            Self::Inconclusive => "INCONCL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub id: &'static str,
    pub question: &'static str,
    /// What we expect, so a surprising result is obvious in the report.
    pub expectation: &'static str,
    pub outcome: Outcome,
    pub detail: String,
}

impl Check {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "question": self.question,
            "expectation": self.expectation,
            "outcome": self.outcome.as_str(),
            "detail": self.detail,
        })
    }
}

/// Run a command, summarising what happened in one line.
fn attempt(program: &Path, args: &[&str]) -> (Outcome, String) {
    attempt_env(program, args, &[])
}

/// As [`attempt`], with extra environment.
///
/// proot needs this: it is not self-contained, but execs a small loader ELF whose
/// path it reads from `PROOT_LOADER`. Without it proot starts and then fails in a
/// way that looks like the SELinux restriction but is not, which would poison the
/// whole result.
fn attempt_env(program: &Path, args: &[&str], env: &[(&str, &str)]) -> (Outcome, String) {
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    match command.output() {
        Ok(out) => {
            let code = out.status.code();
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if out.status.success() {
                (Outcome::Worked, format!("exit 0, stdout={stdout:?}"))
            } else {
                // A dynamic-linking failure is NOT the SELinux restriction.
                // proot is not static, and a missing dependency yields
                // "CANNOT LINK EXECUTABLE", which reads exactly like a denial.
                // Separating them is the difference between an answer and a
                // confidently wrong one.
                let outcome = if stderr.contains("CANNOT LINK EXECUTABLE") {
                    Outcome::Inconclusive
                } else {
                    Outcome::Blocked
                };
                (
                    outcome,
                    format!("exit {code:?}, stdout={stdout:?}, stderr={stderr:?}"),
                )
            }
        }
        Err(e) => {
            // The distinction that matters: EACCES is SELinux refusing; ENOENT is
            // us pointing at nothing. Conflating them would make the whole report
            // untrustworthy.
            let kind = match e.kind() {
                ErrorKind::PermissionDenied => "EACCES (permission denied - the W^X rule)",
                ErrorKind::NotFound => "ENOENT (no such file - a probe bug, not a restriction)",
                _ => "spawn error",
            };
            (
                Outcome::Blocked,
                format!("{kind}: {e} (raw errno {:?})", e.raw_os_error()),
            )
        }
    }
}

fn read_trimmed(path: &str) -> String {
    fs::read_to_string(path)
        .map(|s| s.trim().trim_end_matches('\0').to_string())
        .unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

fn make_executable(path: &Path) -> std::io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

/// Unpack the rootfs, using `mc-sandbox`'s own extractor.
///
/// Deliberately the real implementation rather than a copy: the probe is worth
/// more when a regression in shipped code shows up here.
fn extract(archive: &Path, dest: &Path) -> Result<u64, mc_sandbox::RootfsError> {
    mc_sandbox::rootfs::extract(archive, dest, &|_| {})
}

/// Everything the probe needs from the Android side.
pub struct ProbeInput {
    /// The app's private files directory.
    pub files_dir: PathBuf,
    /// The APK's native library directory - always executable.
    pub native_lib_dir: PathBuf,
    /// `targetSdkVersion`, read from the running app.
    pub target_sdk: i32,
}

pub struct Report {
    pub environment: Value,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn to_json(&self) -> Value {
        json!({
            "environment": self.environment,
            "checks": self.checks.iter().map(Check::to_json).collect::<Vec<_>>(),
        })
    }

    /// A plain-text rendering, for logcat and for the on-screen view.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("=== mobile-coder exec probe ===\n\n");
        out.push_str(&format!(
            "{}\n\n",
            serde_json::to_string_pretty(&self.environment).unwrap_or_default()
        ));
        for c in &self.checks {
            out.push_str(&format!(
                "[{:^7}] {}\n          q: {}\n          expected: {}\n          {}\n\n",
                c.outcome.as_str(),
                c.id,
                c.question,
                c.expectation,
                c.detail
            ));
        }
        out.push_str(&self.verdict());
        out
    }

    /// Turn the results into the decision they imply.
    pub fn verdict(&self) -> String {
        let get = |id: &str| self.checks.iter().find(|c| c.id == id).map(|c| c.outcome);

        let control_ok = get("direct-bionic") == Some(Outcome::Blocked);
        let guest = get("proot-guest");

        let mut v = String::from("--- verdict ---\n");
        if !control_ok {
            v.push_str(
                "CONTROL FAILED: direct exec from app data was NOT blocked. Either targetSdk is \
                 below 29 or SELinux is permissive. Every other result below is measuring the \
                 wrong thing - fix this before believing any of it.\n",
            );
            return v;
        }
        v.push_str("Control holds: direct exec from app data is blocked, as expected.\n");

        if get("mc-sandbox-e2e") == Some(Outcome::Worked) {
            v.push_str("mc-sandbox's own ProotBackend works end to end on this device.\n");
        }

        match guest {
            Some(Outcome::Worked) => v.push_str(
                "PROOT WORKS at this targetSdk. A Linux userland is viable without dropping to \
                 targetSdk 28 - proceed with the current target.\n",
            ),
            Some(Outcome::Inconclusive) => v.push_str(
                "INCONCLUSIVE. proot failed to start - a dynamic-linking failure, not a denial. \
                 This is a fault in the probe's own packaging, NOT a finding about Android. \
                 Fix the missing library and re-run; do not record this as a result.\n",
            ),
            Some(Outcome::Skipped) => v.push_str(
                "INCONCLUSIVE. The end-to-end check did not run - assets are missing. \
                 Run fetch-assets.sh and try again.\n",
            ),
            _ => v.push_str(
                "PROOT CANNOT RUN GUEST BINARIES at this targetSdk. The options are: ship \
                 targetSdk 28 (sideload only), or replace the Linux rootfs with a bionic-native \
                 toolchain. See docs/ARCHITECTURE.md section 2.2.\n",
            ),
        }
        v
    }
}

pub fn run(input: &ProbeInput) -> Report {
    let work = input.files_dir.join("probe");
    let _ = fs::create_dir_all(&work);

    let environment = json!({
        "target_sdk": input.target_sdk,
        "arch": std::env::consts::ARCH,
        "files_dir": input.files_dir.display().to_string(),
        "native_lib_dir": input.native_lib_dir.display().to_string(),
        "selinux_enforce": read_trimmed("/sys/fs/selinux/enforce"),
        "selinux_context": read_trimmed("/proc/self/attr/current"),
    });

    let mut checks = Vec::new();

    // --- control: is the restriction even in force? -----------------------
    let bionic_copy = work.join("sh-bionic");
    let control = match fs::copy("/system/bin/sh", &bionic_copy)
        .and_then(|_| make_executable(&bionic_copy))
    {
        Ok(()) => {
            let (outcome, detail) = attempt(&bionic_copy, &["-c", "echo alive"]);
            // Inverted on purpose: being blocked here is the healthy result.
            Check {
                id: "direct-bionic",
                question: "Direct exec() of a bionic binary copied into app data",
                expectation: "BLOCKED on targetSdk >= 29 - this is the control",
                outcome,
                detail,
            }
        }
        Err(e) => Check {
            id: "direct-bionic",
            question: "Direct exec() of a bionic binary copied into app data",
            expectation: "BLOCKED on targetSdk >= 29 - this is the control",
            outcome: Outcome::Skipped,
            detail: format!("could not stage /system/bin/sh: {e}"),
        },
    };
    checks.push(control);

    // --- does system_linker_exec work at all? -----------------------------
    let (outcome, detail) = if bionic_copy.exists() {
        attempt(
            Path::new(LINKER64),
            &[bionic_copy.to_str().unwrap_or_default(), "-c", "echo alive"],
        )
    } else {
        (Outcome::Skipped, "no staged bionic binary".into())
    };
    checks.push(Check {
        id: "linker-bionic",
        question: "Running that same bionic binary via /system/bin/linker64",
        expectation: "WORKED - this is what termux-exec relies on",
        outcome,
        detail,
    });

    // --- the musl question ------------------------------------------------
    let rootfs = work.join("alpine");
    let tarball = input.files_dir.join("alpine-minirootfs.tar");
    let busybox = rootfs.join("bin/busybox");

    if tarball.exists()
        && !busybox.exists()
        && let Err(e) = extract(&tarball, &rootfs)
    {
        log::warn!("rootfs extraction reported: {e}");
    }

    // Point the linker at the rootfs's own libraries. Without this the failure
    // is just "not found", which says nothing about whether bionic *could* have
    // loaded a musl binary.
    let musl_libs = format!(
        "{}:{}",
        rootfs.join("lib").display(),
        rootfs.join("usr/lib").display()
    );
    let (outcome, mut detail) = if busybox.exists() {
        attempt_env(
            Path::new(LINKER64),
            &[busybox.to_str().unwrap_or_default(), "echo", "alive"],
            &[("LD_LIBRARY_PATH", &musl_libs)],
        )
    } else {
        (
            Outcome::Skipped,
            format!("{} missing - did fetch-assets.sh run?", busybox.display()),
        )
    };
    // Record whether musl's loader is actually on disk, so "not found" can be
    // told apart from "present but refused".
    let musl_on_disk = fs::read_dir(rootfs.join("lib"))
        .map(|d| {
            d.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("libc.musl"))
        })
        .unwrap_or(false);
    detail.push_str(&format!(" | musl libc present on disk: {musl_on_disk}"));
    checks.push(Check {
        id: "linker-musl",
        question: "Running an Alpine (musl) binary via bionic's /system/bin/linker64",
        expectation: "UNKNOWN - this is the question the probe exists to answer",
        outcome,
        detail,
    });

    // --- is the native library dir executable? ----------------------------
    let proot = input.native_lib_dir.join("libproot.so");
    let loader = input.native_lib_dir.join("libproot-loader.so");
    let native_dir = input.native_lib_dir.to_str().unwrap_or_default();
    let mut proot_env: Vec<(&str, &str)> = Vec::new();
    if loader.exists()
        && let Some(path) = loader.to_str()
    {
        proot_env.push(("PROOT_LOADER", path));
    }
    // proot links against libtalloc and libandroid-shmem, which ship beside it
    // in the native library directory. That directory is not on the default
    // search path for a spawned process, so without this the linker cannot find
    // them and proot dies before it can tell us anything.
    proot_env.push(("LD_LIBRARY_PATH", native_dir));

    let (outcome, detail) = if proot.exists() {
        attempt_env(&proot, &["--version"], &proot_env)
    } else {
        (
            Outcome::Skipped,
            format!("{} missing - did fetch-assets.sh run?", proot.display()),
        )
    };
    checks.push(Check {
        id: "nativelib-exec",
        question: "Executing proot straight from the native library directory",
        expectation: "WORKED - jniLibs is executable regardless of targetSdk",
        outcome,
        detail,
    });

    // --- the decisive test ------------------------------------------------
    let (outcome, detail) = if proot.exists() && rootfs.join("bin/busybox").exists() {
        attempt_env(
            &proot,
            &[
                "-r",
                rootfs.to_str().unwrap_or_default(),
                "-b",
                "/dev",
                "-b",
                "/proc",
                "-w",
                "/",
                "/bin/busybox",
                "echo",
                "alive-inside-proot",
            ],
            &proot_env,
        )
    } else {
        (Outcome::Skipped, "needs both proot and the rootfs".into())
    };
    checks.push(Check {
        id: "proot-guest",
        question: "proot running a guest binary from the rootfs, end to end",
        expectation: "THE ANSWER - decides whether targetSdk >= 29 is viable",
        outcome,
        detail,
    });

    // --- the shipped code path, end to end --------------------------------
    //
    // Everything above measures the platform. This measures *us*: the same
    // ProotBackend the app will use, driven through the same Sandbox::run().
    let (outcome, detail) = if proot.exists() && rootfs.join("bin/busybox").exists() {
        let backend = ProotBackend::from_native_lib_dir(&input.native_lib_dir, &rootfs);
        let sandbox = Sandbox::new(Box::new(backend));
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => match runtime.block_on(sandbox.run("echo hi-from-mc-sandbox", None)) {
                Ok(out) if out.ok() => (
                    Outcome::Worked,
                    format!("stdout={:?}", out.stdout.trim()),
                ),
                Ok(out) => {
                    let stderr = out.stderr.trim().to_string();
                    let outcome = if stderr.contains("CANNOT LINK EXECUTABLE") {
                        Outcome::Inconclusive
                    } else {
                        Outcome::Blocked
                    };
                    (outcome, format!("exit {:?}, stderr={stderr:?}", out.status))
                }
                Err(e) => (Outcome::Inconclusive, e.to_string()),
            },
            Err(e) => (Outcome::Skipped, format!("no tokio runtime: {e}")),
        }
    } else {
        (Outcome::Skipped, "needs both proot and the rootfs".into())
    };
    checks.push(Check {
        id: "mc-sandbox-e2e",
        question: "mc_sandbox::Sandbox::run() through ProotBackend - the shipped code path",
        expectation: "WORKED - proves the app's own sandbox works, not just proot",
        outcome,
        detail,
    });

    // --- pty, for mc-pty --------------------------------------------------
    let (outcome, detail) = match fs::OpenOptions::new().read(true).write(true).open("/dev/ptmx") {
        Ok(_) => (Outcome::Worked, "/dev/ptmx opened read-write".into()),
        Err(e) => (Outcome::Blocked, format!("/dev/ptmx: {e}")),
    };
    checks.push(Check {
        id: "devptmx",
        question: "Opening /dev/ptmx, which mc-pty needs for interactive terminals",
        expectation: "WORKED - Termux depends on this",
        outcome,
        detail,
    });

    Report { environment, checks }
}
