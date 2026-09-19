//! Where commands run: the host, or an Alpine/Debian rootfs under proot.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::exec::{ExecError, ExecStrategy, ResolvedCommand};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error("spawning {program} failed: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Result of a non-interactive command - what a tool call gets back.
#[derive(Debug, Clone)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The command was killed for running past its deadline.
    pub timed_out: bool,
}

impl Output {
    pub fn ok(&self) -> bool {
        !self.timed_out && self.status == Some(0)
    }
}

/// Builds the actual command line for a backend.
///
/// Kept as a trait so the desktop dev loop and the on-device path are genuinely
/// interchangeable: `mc-agent` is written against [`Sandbox`] and never learns
/// which one it is talking to.
pub trait Backend: Send + Sync + std::fmt::Debug {
    /// Turn a shell command into a spawnable command.
    fn build(&self, command: &str, cwd: Option<&Path>) -> Result<ResolvedCommand, SandboxError>;
}

/// Runs commands directly on the host. The desktop dev loop, and tests.
#[derive(Debug, Clone)]
pub struct HostBackend {
    pub shell: PathBuf,
}

impl Default for HostBackend {
    fn default() -> Self {
        Self {
            shell: PathBuf::from("/bin/sh"),
        }
    }
}

impl Backend for HostBackend {
    fn build(&self, command: &str, _cwd: Option<&Path>) -> Result<ResolvedCommand, SandboxError> {
        Ok(ExecStrategy::Direct.resolve(&self.shell, ["-lc", command])?)
    }
}

/// Environment handed to the guest.
///
/// Set explicitly rather than inherited. The host environment on Android carries
/// an Android `PATH`, and - more dangerously - the `LD_LIBRARY_PATH` we need for
/// proot itself, pointing at bionic libraries. Letting that reach a musl or glibc
/// guest invites its loader to try to load bionic `.so` files, which fails in
/// ways that look like anything but the actual cause.
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const GUEST_HOME: &str = "/root";
const GUEST_TERM: &str = "xterm-256color";
const GUEST_LANG: &str = "C.UTF-8";

/// Runs commands inside a rootfs under proot.
///
/// The invocation here is not guesswork: every piece of it was established by
/// `tools/exec-probe` running on-device under SELinux enforcement. See
/// `docs/EXEC-PROBE.md` for the measurements.
#[derive(Debug, Clone)]
pub struct ProotBackend {
    /// Absolute path to the proot binary.
    ///
    /// On Android this lives in the APK's native library directory as
    /// `libproot.so`. That directory is executable regardless of
    /// `targetSdkVersion`, which is why proot itself is never the problem.
    pub proot: PathBuf,
    /// proot's loader ELF, passed via `PROOT_LOADER`.
    ///
    /// proot is not self-contained. It hands control to this small loader, which
    /// *maps* guest binaries rather than asking the kernel to `execve` them -
    /// which is precisely how a Linux userland runs at `targetSdk >= 29` while
    /// direct `exec()` from app data stays blocked.
    pub loader: Option<PathBuf>,
    /// Directory holding proot's own shared libraries, for `LD_LIBRARY_PATH`.
    ///
    /// proot links against `libtalloc` and `libandroid-shmem`. The native library
    /// directory is not on the default search path for a spawned process, so
    /// without this proot dies with `CANNOT LINK EXECUTABLE` - a failure that
    /// reads exactly like an SELinux denial and is not one.
    pub lib_dir: Option<PathBuf>,
    /// Writable scratch directory for proot, passed via `PROOT_TMP_DIR`.
    ///
    /// Required with the Termux proot build we ship: it has Termux's own
    /// `/data/data/com.termux/files/usr/tmp` compiled in as the default, which
    /// does not exist in this app. Without an override, every command writes
    /// two proot warnings to stderr - noise the agent then reads as an error.
    pub tmp_dir: Option<PathBuf>,
    /// Host path to the extracted rootfs.
    pub rootfs: PathBuf,
    /// Guest working directory.
    pub cwd: PathBuf,
    /// Guest shell.
    pub shell: PathBuf,
    /// Present as uid 0 inside the guest, so package managers work.
    pub root_id: bool,
}

impl ProotBackend {
    pub fn new(proot: impl Into<PathBuf>, rootfs: impl Into<PathBuf>) -> Self {
        Self {
            proot: proot.into(),
            loader: None,
            lib_dir: None,
            tmp_dir: None,
            rootfs: rootfs.into(),
            cwd: PathBuf::from(GUEST_HOME),
            shell: PathBuf::from("/bin/sh"),
            root_id: true,
        }
    }

    /// Build from Android's `nativeLibraryDir`, where the APK's native libraries
    /// are unpacked. This is the constructor the app should use: it wires up
    /// proot, its loader and its library path together, so they cannot drift
    /// apart.
    pub fn from_native_lib_dir(dir: impl AsRef<Path>, rootfs: impl Into<PathBuf>) -> Self {
        let dir = dir.as_ref();
        let loader = dir.join("libproot-loader.so");
        Self {
            proot: dir.join("libproot.so"),
            loader: loader.exists().then_some(loader),
            lib_dir: Some(dir.to_path_buf()),
            ..Self::new(dir.join("libproot.so"), rootfs)
        }
    }

    /// Give proot a scratch directory, creating it if needed.
    pub fn with_tmp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), %e, "could not create proot tmp dir");
        }
        self.tmp_dir = Some(dir);
        self
    }
}

impl ProotBackend {
    /// The command for an interactive login shell inside the guest - what the
    /// terminal pane runs. Same proot setup as a tool call, minus `-c`.
    pub fn interactive(&self) -> Result<ResolvedCommand, SandboxError> {
        self.resolve_with(&self.cwd, &[OsString::from("-l")])
    }

    /// proot invocation ending in the guest shell with `shell_args`.
    fn resolve_with(&self, cwd: &Path, shell_args: &[OsString]) -> Result<ResolvedCommand, SandboxError> {
        let mut args: Vec<OsString> = Vec::new();
        args.push("-r".into());
        args.push(self.rootfs.clone().into());
        if self.root_id {
            args.push("-0".into());
        }
        // Android denies `link()` on app data files, so any package that ships
        // hard links installs partially - measured on a Galaxy Z Fold6, where
        // `apk add build-base` left `/usr/bin/gcc` (a hard link in Alpine's gcc
        // package) missing while its target landed fine. proot emulates hard
        // links with symlinks under this flag.
        args.push("--link2symlink".into());
        // Take the whole guest process tree down with proot, so a timed-out or
        // cancelled tool call cannot leave orphans running on a battery.
        args.push("--kill-on-exit".into());
        for bind in ["/dev", "/proc", "/sys"] {
            args.push("-b".into());
            args.push(bind.into());
        }
        args.push("-w".into());
        args.push(cwd.into());

        // Scrub the environment at the guest boundary with `env -i`, then set
        // only what the guest should see. See GUEST_* above for why inheriting
        // would be actively harmful rather than merely untidy.
        args.push("/usr/bin/env".into());
        args.push("-i".into());
        args.push(format!("HOME={GUEST_HOME}").into());
        args.push(format!("PATH={GUEST_PATH}").into());
        args.push(format!("TERM={GUEST_TERM}").into());
        args.push(format!("LANG={GUEST_LANG}").into());

        args.push(self.shell.clone().into());
        args.extend(shell_args.iter().cloned());

        // NOTE: `Direct` is correct here, and the reason is worth stating.
        //
        // proot lives in the native library directory, so executing *it* is
        // always allowed. The W^X problem is about the binaries proot goes on to
        // execute inside the rootfs - and we cannot reach those from out here,
        // because proot performs those `execve` calls itself, deep inside its own
        // ptrace loop. Wrapping this outer command in the system linker would
        // therefore accomplish precisely nothing for the guest.
        //
        // Making guest exec work under `targetSdkVersion >= 29` is a property of
        // the proot build, not of this call site. `tools/exec-probe` measures
        // whether it holds. See docs/EXEC-PROBE.md.
        let mut resolved = ExecStrategy::Direct.resolve(&self.proot, args)?;

        // Host-side environment for proot itself.
        if let Some(loader) = &self.loader {
            resolved
                .env
                .push(("PROOT_LOADER".to_string(), loader.clone().into()));
        }
        if let Some(lib_dir) = &self.lib_dir {
            resolved
                .env
                .push(("LD_LIBRARY_PATH".to_string(), lib_dir.clone().into()));
        }
        if let Some(tmp_dir) = &self.tmp_dir {
            resolved
                .env
                .push(("PROOT_TMP_DIR".to_string(), tmp_dir.clone().into()));
        }
        Ok(resolved)
    }
}

impl Backend for ProotBackend {
    fn build(&self, command: &str, cwd: Option<&Path>) -> Result<ResolvedCommand, SandboxError> {
        let cwd = cwd.unwrap_or(&self.cwd);
        self.resolve_with(cwd, &[OsString::from("-lc"), OsString::from(command)])
    }
}

/// Makes a [`Sandbox`] on demand.
///
/// A `Sandbox` is not `Clone` and is cheap to build, so long-lived things hold
/// one of these instead: the UI, which needs one whenever a button is pressed,
/// and the agent worker, which builds a fresh one per turn. On Android it is
/// only available once the rootfs is installed, which is why it can fail.
pub trait SandboxFactory: Send + Sync + std::fmt::Debug {
    fn create(&self) -> Result<Sandbox, String>;

    /// Where cloned projects live, as a path *inside* this sandbox.
    ///
    /// On a phone that is a guest path under the rootfs; on the desktop dev
    /// loop the sandbox is the machine itself, where `/root` is neither
    /// writable nor anybody's idea of a good place for a checkout.
    fn projects_dir(&self) -> String {
        "/root/projects".to_string()
    }
}

/// The sandbox the rest of the app talks to.
#[derive(Debug)]
pub struct Sandbox {
    backend: Box<dyn Backend>,
}

impl Sandbox {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self { backend }
    }

    /// The desktop dev loop: no proot, no rootfs, just the host shell.
    pub fn host() -> Self {
        Self::new(Box::new(HostBackend::default()))
    }

    /// Run a command to completion and capture its output.
    ///
    /// This is the shape a tool call wants. Interactive work wants a live PTY
    /// instead - see `mc-pty`, which is a separate crate precisely so these two
    /// needs stop compromising each other.
    pub async fn run(&self, command: &str, cwd: Option<&Path>) -> Result<Output, SandboxError> {
        self.run_with_timeout(command, cwd, None).await
    }

    /// As [`Self::run_with_timeout`], also killed when `cancel` fires.
    pub async fn run_cancellable(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout: Option<std::time::Duration>,
        cancel: &mc_core::Cancel,
    ) -> Result<Output, SandboxError> {
        self.run_inner(command, cwd, timeout, Some(cancel)).await
    }

    /// As [`Self::run`], killed if it outlives `timeout`.
    ///
    /// Without a deadline a single mistake - a command waiting on input, a test
    /// that hangs, a `tail -f` - blocks the agent for as long as the app lives,
    /// with no way to intervene from a phone. What is killed is the whole
    /// process group, not just the process that was spawned: see
    /// [`Self::run_inner`].
    pub async fn run_with_timeout(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout: Option<std::time::Duration>,
    ) -> Result<Output, SandboxError> {
        self.run_inner(command, cwd, timeout, None).await
    }

    /// Start a command that keeps running after this turn ends.
    ///
    /// Returns the job id at once; the output is collected in the background
    /// and read later through [`crate::jobs`]. Unlike [`Self::run_with_timeout`]
    /// there is no deadline - that is the whole point of a job - so the ways to
    /// end one are `jobs::kill`, `jobs::kill_all`, and the app exiting.
    pub fn start_background(
        &self,
        command: &str,
        cwd: Option<&Path>,
    ) -> Result<String, SandboxError> {
        let resolved = self.backend.build(command, cwd)?;
        let program = resolved.program.display().to_string();
        crate::jobs::start(command.to_string(), configure(&resolved))
            .map_err(|source| SandboxError::Spawn { program, source })
    }

    #[allow(rustdoc::private_intra_doc_links)]
    async fn run_inner(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout: Option<std::time::Duration>,
        cancel: Option<&mc_core::Cancel>,
    ) -> Result<Output, SandboxError> {
        let resolved = self.backend.build(command, cwd)?;
        let program = resolved.program.display().to_string();
        let spawn_error = |source| SandboxError::Spawn { program: program.clone(), source };
        let mut child = configure(&resolved).spawn().map_err(spawn_error)?;

        // Drain both pipes while waiting. Waiting on the process alone
        // deadlocks as soon as it writes more than a pipe buffer (~64 KB) -
        // which any verbose build does - because nothing is emptying the pipe.
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let read_out = tokio::spawn(async move {
            let mut buffer = Vec::new();
            if let Some(pipe) = stdout.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_end(pipe, &mut buffer).await;
            }
            buffer
        });
        let read_err = tokio::spawn(async move {
            let mut buffer = Vec::new();
            if let Some(pipe) = stderr.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_end(pipe, &mut buffer).await;
            }
            buffer
        });

        // Wait for whichever comes first: the command finishing, its deadline,
        // or the user pressing stop.
        let deadline = async {
            match timeout {
                Some(limit) => tokio::time::sleep(limit).await,
                None => std::future::pending().await,
            }
        };
        let stopped = async {
            match cancel {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending().await,
            }
        };

        let (status, timed_out, cancelled) = tokio::select! {
            status = child.wait() => (Some(status.map_err(spawn_error)?), false, false),
            _ = deadline => {
                kill_group(&child);
                let _ = child.start_kill();
                (child.wait().await.ok(), true, false)
            }
            _ = stopped => {
                kill_group(&child);
                let _ = child.start_kill();
                (child.wait().await.ok(), false, true)
            }
        };
        let _ = cancelled;

        // Bounded. Once the command is gone its pipes should close at once, but
        // a process that escaped the group - one that made a session of its own,
        // say - would hold them open and hang the turn behind a command that is
        // already dead. Losing the last few bytes of output is the better trade.
        let drain = |reader| async {
            match tokio::time::timeout(DRAIN_GRACE, reader).await {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(_)) | Err(_) => Vec::new(),
            }
        };

        Ok(Output {
            status: status.and_then(|s| s.code()),
            stdout: String::from_utf8_lossy(&drain(read_out).await).into_owned(),
            stderr: String::from_utf8_lossy(&drain(read_err).await).into_owned(),
            timed_out,
        })
    }
}

/// Build the child process for a resolved command.
///
/// Every command the agent runs goes through here, foreground or background,
/// because each of these settings was a bug first.
fn configure(resolved: &ResolvedCommand) -> tokio::process::Command {
    let mut proc = tokio::process::Command::new(&resolved.program);
    proc.args(&resolved.args);
    for (key, value) in &resolved.env {
        proc.env(key, value);
    }
    // Piped, not inherited: the output is the tool result, and a child writing
    // to the app's own stdout would land in logcat instead.
    proc.stdout(std::process::Stdio::piped());
    proc.stderr(std::process::Stdio::piped());
    proc.stdin(std::process::Stdio::null());

    // Its own process group, so a deadline, a stop or a job kill can end
    // everything the command started rather than only the process spawned here.
    //
    // proot's `--kill-on-exit` does not cover this: it kills the guest tree
    // when the *guest's* first process exits, and does nothing when proot
    // itself is killed - its tracees are simply detached. Measured on the
    // Fold6: `sleep 600` under a 5-second deadline outlived the kill with
    // PPID 1, and because it still held the write end of stdout, the reader
    // never saw EOF and the tool never returned at all.
    #[cfg(unix)]
    proc.process_group(0);

    // Hand the command a clean signal environment.
    //
    // Both halves of this were measured on the Fold6, where
    // `yes hello | head -c 2000000 | wc -c` printed its answer and then sat
    // there: busybox `yes` loops on the write error that a missing SIGPIPE
    // leaves behind, and only a signal ends it. The same pipeline from a
    // shell takes a second.
    //
    // Ignored dispositions survive exec, and Rust ignores SIGPIPE for the
    // whole process at startup. Blocked signals survive exec too, and an
    // Android app's threads run with SIGPIPE (among others) blocked - so
    // resetting the disposition alone was not enough, and the guest still
    // saw `EPIPE` where it expected to die.
    #[cfg(unix)]
    // SAFETY: both calls are async-signal-safe and touch nothing outside this
    // child's own signal state, between fork and exec.
    unsafe {
        proc.pre_exec(|| {
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            Ok(())
        });
    }

    proc
}

/// How long to keep reading a command's output after the command itself has
/// finished or been killed.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Kill everything in the child's process group.
///
/// The child leads its own group (see `run_inner`), so this reaches the shell,
/// proot, and whatever they started - a `make -j8`, a server left in the
/// background - none of which a signal to the child alone would touch.
#[cfg(unix)]
fn kill_group(child: &tokio::process::Child) {
    let Some(pid) = child.id() else { return };
    // SAFETY: a plain signal to a process group this process created. A pid
    // that has already been reaped cannot be reused here, because the child has
    // not been waited on yet.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_child: &tokio::process::Child) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(cmd: &ResolvedCommand) -> Vec<String> {
        cmd.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn proot_binds_dev_proc_sys_and_lands_in_the_requested_cwd() {
        let backend = ProotBackend::new("/lib/libproot.so", "/data/rootfs");
        let cmd = backend.build("cargo build", Some(Path::new("/work"))).unwrap();

        assert_eq!(cmd.program, PathBuf::from("/lib/libproot.so"));
        let args = args_of(&cmd);
        assert_eq!(args[0], "-r");
        assert_eq!(args[1], "/data/rootfs");
        assert!(args.windows(2).any(|w| w == ["-b", "/dev"]));
        assert!(args.windows(2).any(|w| w == ["-w", "/work"]));
        assert_eq!(args.last().unwrap(), "cargo build");
    }

    #[test]
    fn native_lib_dir_wires_up_loader_and_library_path_together() {
        // Regression guard for the exact failure the probe hit: proot starting
        // and then dying with CANNOT LINK EXECUTABLE, which is indistinguishable
        // from an SELinux denial unless you already know to look.
        let dir = std::env::temp_dir().join("mc-nativelibs-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("libproot-loader.so"), b"x").unwrap();

        let backend = ProotBackend::from_native_lib_dir(&dir, "/data/rootfs");
        let cmd = backend.build("true", None).unwrap();
        let env: Vec<(String, String)> = cmd
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string_lossy().into_owned()))
            .collect();

        assert!(
            env.iter().any(|(k, v)| k == "PROOT_LOADER"
                && v.ends_with("libproot-loader.so")),
            "PROOT_LOADER missing: {env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "LD_LIBRARY_PATH" && v == &dir.display().to_string()),
            "LD_LIBRARY_PATH missing: {env:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tmp_dir_is_passed_to_proot_and_created() {
        let tmp = std::env::temp_dir().join("mc-proot-tmp-test");
        let _ = std::fs::remove_dir_all(&tmp);
        let backend = ProotBackend::new("/lib/libproot.so", "/data/rootfs").with_tmp_dir(&tmp);
        assert!(tmp.is_dir(), "with_tmp_dir should create the directory");

        let cmd = backend.build("true", None).unwrap();
        assert!(
            cmd.env
                .iter()
                .any(|(k, v)| k == "PROOT_TMP_DIR" && v == tmp.as_os_str()),
            "PROOT_TMP_DIR missing: {:?}",
            cmd.env
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_guest_environment_is_scrubbed_not_inherited() {
        // LD_LIBRARY_PATH is needed by proot on the host and must NOT reach a
        // musl or glibc guest, whose loader would try to load bionic libraries.
        let backend = ProotBackend::new("/lib/libproot.so", "/data/rootfs");
        let args = args_of(&backend.build("true", None).unwrap());

        let env_at = args.iter().position(|a| a == "/usr/bin/env").expect("env -i");
        assert_eq!(args[env_at + 1], "-i");
        assert!(args.iter().any(|a| a.starts_with("PATH=/usr/local/sbin")));
        assert!(
            !args.iter().any(|a| a.starts_with("LD_LIBRARY_PATH=")),
            "host LD_LIBRARY_PATH must not be handed to the guest"
        );
    }

    #[test]
    fn hard_links_are_emulated_because_android_forbids_them() {
        // Without this, `apk add build-base` silently leaves out /usr/bin/gcc.
        let backend = ProotBackend::new("/lib/libproot.so", "/data/rootfs");
        let args = args_of(&backend.build("true", None).unwrap());
        assert!(args.contains(&"--link2symlink".to_string()), "{args:?}");
        assert!(args.contains(&"--kill-on-exit".to_string()), "{args:?}");
    }

    #[test]
    fn the_interactive_shell_shares_the_tool_setup_but_takes_no_command() {
        let backend = ProotBackend::new("/lib/libproot.so", "/data/rootfs");
        let interactive = args_of(&backend.interactive().unwrap());
        let tool = args_of(&backend.build("true", None).unwrap());

        assert_eq!(interactive.last().unwrap(), "-l", "a login shell, no -c");
        assert!(!interactive.contains(&"-lc".to_string()));
        // Everything up to the shell is identical, so the terminal and the
        // agent see the same guest.
        let shell_at = tool.iter().position(|a| a == "/bin/sh").unwrap();
        assert_eq!(interactive[..=shell_at], tool[..=shell_at]);
    }

    #[test]
    fn root_id_is_requested_so_guest_package_managers_work() {
        let backend = ProotBackend::new("/lib/libproot.so", "/data/rootfs");
        assert!(args_of(&backend.build("apk add git", None).unwrap()).contains(&"-0".to_string()));
    }

    #[tokio::test]
    async fn a_command_that_overstays_is_killed_and_says_so() {
        let sandbox = Sandbox::host();
        let started = std::time::Instant::now();
        let out = sandbox
            .run_with_timeout("sleep 30", None, Some(std::time::Duration::from_millis(300)))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert!(!out.ok(), "a killed command must not look successful");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "returned promptly");
    }

    #[tokio::test]
    async fn output_written_before_the_deadline_is_kept() {
        let out = Sandbox::host()
            .run_with_timeout(
                "echo partial; sleep 30",
                None,
                Some(std::time::Duration::from_millis(500)),
            )
            .await
            .unwrap();
        assert!(out.timed_out);
        assert!(out.stdout.contains("partial"), "got {:?}", out.stdout);
    }

    #[tokio::test]
    async fn stop_kills_a_running_command() {
        let cancel = mc_core::Cancel::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            stopper.cancel();
        });
        let started = std::time::Instant::now();
        let out = Sandbox::host()
            .run_cancellable("sleep 30", None, None, &cancel)
            .await
            .unwrap();
        assert!(!out.ok(), "a killed command is not a success");
        assert!(!out.timed_out, "it was stopped, not timed out");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "stopped promptly");
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn a_command_does_not_inherit_rusts_ignored_sigpipe() {
        // Regression, found on the phone: Rust ignores SIGPIPE process-wide, an
        // ignored signal survives exec, and so `yes hello | head -c 2000000`
        // never ended - busybox `yes` loops on the write error that replaced
        // the signal. The kernel reports the mask, so ask it rather than trying
        // to provoke the hang.
        let out = Sandbox::host()
            .run_with_timeout(
                "grep -E '^SigIgn|^SigBlk' /proc/self/status",
                None,
                Some(std::time::Duration::from_secs(10)),
            )
            .await
            .unwrap();
        // Signal 13 is SIGPIPE; these masks are 0-based, so bit 12.
        const SIGPIPE_BIT: u64 = 1 << 12;
        let mut seen = 0;
        for line in out.stdout.lines() {
            let (name, hex) = line.split_once('\t').unwrap_or_else(|| {
                panic!("unexpected status line: {line:?}");
            });
            let mask = u64::from_str_radix(hex.trim(), 16)
                .unwrap_or_else(|_| panic!("unexpected mask: {hex:?}"));
            // Blocked is as fatal as ignored: either way the write returns an
            // error instead of ending the process.
            assert_eq!(mask & SIGPIPE_BIT, 0, "{name} has SIGPIPE set ({mask:#x})");
            seen += 1;
        }
        assert_eq!(seen, 2, "expected both masks: {:?}", out.stdout);
    }

    #[tokio::test]
    async fn a_deadline_kills_what_the_command_left_running() {
        // Regression, found on the phone: killing only the spawned process left
        // its children alive, and one of them still held stdout - so the call
        // did not return when the deadline passed, it returned when that
        // orphan felt like exiting. `sleep 600` outlived a 5-second timeout.
        let dir = std::env::temp_dir().join(format!("mc-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("survived");

        let started = std::time::Instant::now();
        let out = Sandbox::host()
            .run_with_timeout(
                // A background grandchild that outlives the deadline, keeps the
                // pipe open, and leaves proof if it is still running later.
                &format!(
                    "(sleep 3; touch {}) & sleep 30",
                    marker.display()
                ),
                None,
                Some(std::time::Duration::from_millis(500)),
            )
            .await
            .unwrap();

        assert!(out.timed_out);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "returned at the deadline, not when the orphan exited ({:?})",
            started.elapsed(),
        );

        // Long enough that the grandchild would have written its marker.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(!marker.exists(), "a process outlived the deadline");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        // Regression: waiting on the child without draining its pipes hung
        // until the deadline as soon as output passed ~64 KB.
        let started = std::time::Instant::now();
        let out = Sandbox::host()
            .run_with_timeout(
                "yes hello | head -50000",
                None,
                Some(std::time::Duration::from_secs(30)),
            )
            .await
            .unwrap();
        assert!(out.ok(), "timed_out={} status={:?}", out.timed_out, out.status);
        assert!(out.stdout.len() > 64 * 1024, "wrote {} bytes", out.stdout.len());
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "finished promptly");
    }

    #[tokio::test]
    async fn a_command_finishing_inside_the_deadline_is_untouched() {
        let out = Sandbox::host()
            .run_with_timeout("echo quick", None, Some(std::time::Duration::from_secs(10)))
            .await
            .unwrap();
        assert!(out.ok());
        assert!(!out.timed_out);
        assert_eq!(out.stdout.trim(), "quick");
    }

    #[tokio::test]
    async fn host_sandbox_actually_runs_something() {
        let out = Sandbox::host().run("echo mobile-coder", None).await.unwrap();
        assert!(out.ok(), "stderr: {}", out.stderr);
        assert_eq!(out.stdout.trim(), "mobile-coder");
    }
}

impl From<ResolvedCommand> for mc_core::ShellCommand {
    fn from(resolved: ResolvedCommand) -> Self {
        mc_core::ShellCommand {
            program: resolved.program,
            args: resolved.args,
            env: resolved.env,
            cwd: None,
        }
    }
}

/// The user's own shell, in a directory - the desktop dev loop's terminal.
#[derive(Debug, Clone)]
pub struct HostShell {
    pub cwd: PathBuf,
}

impl mc_core::ShellLauncher for HostShell {
    fn command(&self) -> Result<mc_core::ShellCommand, String> {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
        Ok(mc_core::ShellCommand {
            program: PathBuf::from(shell),
            args: vec![OsString::from("-l")],
            env: vec![("TERM".into(), "xterm-256color".into())],
            cwd: Some(self.cwd.clone()),
        })
    }
}
