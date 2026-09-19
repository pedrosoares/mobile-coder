//! The on-device agent: one worker, shared by the chat screen and adb prompts.
//!
//! Nothing here runs a turn on its own. A turn costs money (or GPU time on a
//! local server) and battery, so it starts only when the user sends a message
//! or a developer sends a prompt intent.

use std::{
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use mc_agent::worker;
use mc_core::{AgentHandle, Event, EventBus, Project};
use mc_sandbox::{ProotBackend, Sandbox};

/// Set once the rootfs is installed and proot is known to work.
///
/// A `Sandbox` is not `Clone`, and building one is cheap, so the ingredients are
/// stored rather than the thing itself.
static SANDBOX_PATHS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

/// The worker, created at launch so the chat has a handle before the sandbox is
/// ready. Prompts sent too early get a clear "still installing" reply.
static HANDLE: Mutex<Option<AgentHandle>> = Mutex::new(None);

/// Whether a turn is running. Read by the native message box, which disables
/// Send meanwhile - the same rule the Freya composer applies on desktop.
static BUSY: AtomicBool = AtomicBool::new(false);

pub fn is_busy() -> bool {
    BUSY.load(Ordering::Relaxed)
}

/// What is running, in one line, or empty when nothing is.
///
/// The shell polls this to decide whether the app must be kept out of Android's
/// freezer, and what to put in the notification while it is. Both answers come
/// from the same place so they cannot disagree - a notification saying "1
/// background job" while nothing runs would be worse than no notification.
pub fn work_summary() -> String {
    let jobs = mc_core::jobs::running();
    let turn = is_busy();
    let mut parts = Vec::new();
    if turn {
        parts.push("Working on a turn".to_string());
    }
    match jobs {
        0 => {}
        1 => parts.push("1 background job".into()),
        n => parts.push(format!("{n} background jobs")),
    }
    // A server the user started by hand in the Terminal is not in this list,
    // and cannot be: detecting it would mean reading `/proc/net/tcp` for a
    // listening socket, and this app is not allowed to
    // (measured on the Fold6: "Permission denied", from the app's own process).
    // That case is what the keep-awake switch in settings is for. A server the
    // *agent* starts is a background job, and counted above.
    parts.join(" · ")
}

/// Called by the bootstrap once the guest is proven to run commands.
pub fn mark_ready(native_lib_dir: PathBuf, rootfs: PathBuf) {
    // Write DNS before announcing readiness, so the first agent turn can resolve.
    crate::network::apply(&rootfs);
    let _ = SANDBOX_PATHS.set((native_lib_dir, rootfs));
}

/// `(native_lib_dir, rootfs)`, once the sandbox is ready.
pub fn sandbox_paths() -> Option<&'static (PathBuf, PathBuf)> {
    SANDBOX_PATHS.get()
}

/// Where the conversations are kept, beside the rootfs: one file per chat.
pub const SESSIONS_DIR: &str = "sessions";
/// The single file used before chats were plural. Adopted into the directory on
/// first launch and then gone.
pub const LEGACY_SESSION_FILE: &str = "session.json";

/// The chat library for this device.
pub fn library(files_dir: &std::path::Path) -> mc_core::SessionLibrary {
    let library = mc_core::SessionLibrary::new(files_dir.join(SESSIONS_DIR));
    library.adopt(&files_dir.join(LEGACY_SESSION_FILE));
    library
}

/// Start the worker and return the handle the UI drives it with.
pub fn start(files_dir: PathBuf) -> AgentHandle {
    let bus = EventBus::default();
    mirror_to_logcat(&bus);

    let handle = worker::spawn_with_library(
        bus,
        Project { name: "root".into(), path: PathBuf::from("/root") },
        Box::new(|| {
            let Some(config) = crate::credentials::agent_config() else {
                return Err(
                    "No model is configured. From a computer: apps/android/agent.sh --key sk-ant-... \
                     or --endpoint http://<host>:1234 --model <model>."
                        .into(),
                );
            };
            let Some((lib_dir, rootfs)) = SANDBOX_PATHS.get() else {
                return Err(
                    "The Linux environment is still installing. Try again in a moment.".into(),
                );
            };
            let tmp_dir = rootfs
                .parent()
                .map(|files| files.join("proot-tmp"))
                .unwrap_or_else(|| rootfs.join("tmp"));
            let sandbox = Sandbox::new(Box::new(
                ProotBackend::from_native_lib_dir(lib_dir, rootfs).with_tmp_dir(tmp_dir),
            ));
            log::info!("[turn] endpoint {} model={}", config.base_url, config.model);
            Ok((config, sandbox))
        }),
        Some(files_dir.join(SESSIONS_DIR)),
    );

    if let Ok(mut slot) = HANDLE.lock() {
        *slot = Some(handle.clone());
    }
    handle
}

/// File browsing over the rootfs, available once the bootstrap has installed it.
///
/// The UI gets this at launch, before the rootfs exists, so each call checks
/// readiness and explains itself instead of failing obscurely.
pub struct DeviceFiles;

impl mc_core::FileBrowser for DeviceFiles {
    fn home(&self) -> String {
        "/root".into()
    }

    fn list(&self, path: &str) -> Result<Vec<mc_core::FileEntry>, String> {
        self.inner()?.list(path)
    }

    fn preview(&self, path: &str, limit: usize) -> Result<mc_core::FilePreview, String> {
        self.inner()?.preview(path, limit)
    }
}

impl DeviceFiles {
    fn inner(&self) -> Result<mc_sandbox::fs::RootedFs, String> {
        let (_, rootfs) = SANDBOX_PATHS
            .get()
            .ok_or("The Linux environment is still installing. Try again in a moment.")?;
        Ok(mc_sandbox::fs::RootedFs::new(rootfs, "/root"))
    }
}

/// A sandbox for whoever needs one outside a turn - the Git pane, running git
/// in the guest where the working tree is.
#[derive(Debug)]
pub struct DeviceSandbox;

impl mc_sandbox::SandboxFactory for DeviceSandbox {
    fn create(&self) -> Result<Sandbox, String> {
        let (lib_dir, rootfs) = SANDBOX_PATHS
            .get()
            .ok_or("The Linux environment is still installing. Try again in a moment.")?;
        let tmp_dir = rootfs
            .parent()
            .map(|files| files.join("proot-tmp"))
            .unwrap_or_else(|| rootfs.join("tmp"));
        Ok(Sandbox::new(Box::new(
            ProotBackend::from_native_lib_dir(lib_dir, rootfs).with_tmp_dir(tmp_dir),
        )))
    }
}

/// Shell for the Terminal pane: an interactive login shell under proot, with the
/// exact setup the agent's tool calls use.
pub struct DeviceShell;

impl mc_core::ShellLauncher for DeviceShell {
    fn command(&self) -> Result<mc_core::ShellCommand, String> {
        let (lib_dir, rootfs) = SANDBOX_PATHS
            .get()
            .ok_or("The Linux environment is still installing. Try again in a moment.")?;
        let tmp_dir = rootfs
            .parent()
            .map(|files| files.join("proot-tmp"))
            .unwrap_or_else(|| rootfs.join("tmp"));
        ProotBackend::from_native_lib_dir(lib_dir, rootfs)
            .with_tmp_dir(tmp_dir)
            .interactive()
            .map(Into::into)
            .map_err(|e| e.to_string())
    }
}

/// Stop the turn that is running, if any.
pub fn stop_turn() {
    if let Some(handle) = HANDLE.lock().ok().and_then(|slot| slot.clone()) {
        handle.stop();
    }
}

/// Submit a prompt from outside the UI - an adb intent. It lands in the same
/// conversation as the chat screen, and shows up there.
pub fn run_prompt(prompt: String) {
    match HANDLE.lock().ok().and_then(|slot| slot.clone()) {
        Some(handle) => {
            handle.submit(prompt);
        }
        None => log::error!("the agent worker is not running yet"),
    }
}

/// Echo the conversation to logcat, so adb-driven testing can follow a turn
/// without a screen. Text deltas are buffered into lines first.
fn mirror_to_logcat(bus: &EventBus) {
    let mut events = bus.subscribe();
    std::thread::Builder::new()
        .name("mc-agent-log".into())
        .spawn(move || {
            let mut line = String::new();
            loop {
                let event = match events.blocking_recv() {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                match &event {
                    Event::TurnStarted { .. } => BUSY.store(true, Ordering::Relaxed),
                    Event::TurnEnded { .. } | Event::Failed { .. } => {
                        BUSY.store(false, Ordering::Relaxed)
                    }
                    _ => {}
                }
                match event {
                    Event::TurnStarted { prompt, .. } => log::info!("[turn] starting: {prompt}"),
                    Event::TextDelta { text, .. } => {
                        line.push_str(&text);
                        while let Some(at) = line.find('\n') {
                            let rest = line.split_off(at + 1);
                            log::info!("[claude] {}", line.trim_end());
                            line = rest;
                        }
                    }
                    Event::ToolRequested { name, input, .. } => log::info!("[tool] {name} {input}"),
                    Event::ToolCompleted { is_error, output, .. } => {
                        let head: String = output.chars().take(600).collect();
                        // One log call per line: logcat filters per line, so a
                        // multi-line message loses its tail to any grep.
                        for (i, text) in head.lines().enumerate() {
                            let tag = match (i, is_error) {
                                (0, true) => "failed",
                                (0, false) => "ok",
                                _ => "  |",
                            };
                            if is_error {
                                log::error!("[tool] {tag}: {text}");
                            } else {
                                log::info!("[tool] {tag}: {text}");
                            }
                        }
                    }
                    Event::StreamRetrying { attempt, reason, .. } => {
                        line.clear();
                        log::warn!("[turn] stream broke ({reason}); retry {attempt}");
                    }
                    Event::TurnEnded { stop_reason, .. } => {
                        if !line.is_empty() {
                            log::info!("[claude] {}", line.trim_end());
                            line.clear();
                        }
                        log::info!("[turn] complete: {stop_reason}");
                    }
                    Event::Failed { message, .. } => log::error!("[turn] error: {message}"),
                    _ => {}
                }
            }
        })
        .expect("failed to spawn the log mirror thread");
}

#[cfg(target_os = "android")]
mod jni_bridge {
    use jni::{
        EnvUnowned,
        objects::{JClass, JString},
        sys::jfloat,
    };

    /// `MainActivity.nativeRunPrompt`.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeRunPrompt<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        prompt: JString<'caller>,
    ) {
        let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
            super::run_prompt(prompt.try_to_string(env)?);
            Ok(())
        });
        outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
    }

    /// `MainActivity.nativeStopTurn`.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeStopTurn<'caller>(
        _unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) {
        super::stop_turn();
    }

    /// `MainActivity.nativeIsBusy`.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeIsBusy<'caller>(
        _unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> jni::sys::jboolean {
        super::is_busy()
    }

    /// `MainActivity.nativeTakeSettingsRequest`: true once, after the user taps
    /// the settings button in the app bar.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeTakeSettingsRequest<'caller>(
        _unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> jni::sys::jboolean {
        mc_ui::safe_area::take_settings_request()
    }

    /// `MainActivity.nativeTakeClipboard`: text the app wants copied, or "".
    ///
    /// Pulled by the UI thread rather than pushed from Rust: `ClipboardManager`
    /// is only usable from the thread that owns the Activity, and Freya's
    /// render thread is not it.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeTakeClipboard<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> jni::sys::jstring {
        // The local reference travels back to Java as an address: `resolve`
        // needs a `Default` type to fall back to on error, and a raw pointer
        // has none - 0 is the null JNI returns for "nothing to copy".
        let reference = unowned_env
            .with_env(|env| -> Result<usize, jni::errors::Error> {
                let text = mc_ui::clipboard::take_pending().unwrap_or_default();
                Ok(env.new_string(text)?.into_raw() as usize)
            })
            .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
        reference as jni::sys::jstring
    }

    /// `MainActivity.nativeTakePromptRequest`: which field the Git pane wants,
    /// or 0 for none.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeTakePromptRequest<'caller>(
        _unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> jni::sys::jint {
        mc_ui::prompt::take_request().map(|p| p as jni::sys::jint).unwrap_or(0)
    }

    /// `MainActivity.nativePromptSpec`: how to label the dialog, NUL-separated
    /// as `title\0hint\0secret\0multiline`.
    ///
    /// The wording lives with the screen that asks for it rather than being
    /// duplicated in Kotlin, where it would drift.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativePromptSpec<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        kind: jni::sys::jint,
    ) -> jni::sys::jstring {
        let spec = match mc_ui::prompt::Prompt::from_code(kind as u8) {
            Some(prompt) => format!(
                "{}\0{}\0{}\0{}",
                prompt.title(),
                prompt.hint(),
                u8::from(prompt.is_secret()),
                u8::from(matches!(prompt, mc_ui::prompt::Prompt::CommitMessage)),
            ),
            None => String::new(),
        };
        let reference = unowned_env
            .with_env(|env| -> Result<usize, jni::errors::Error> {
                Ok(env.new_string(spec)?.into_raw() as usize)
            })
            .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
        reference as jni::sys::jstring
    }

    /// `MainActivity.nativeAnswerPrompt`: what the user typed.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeAnswerPrompt<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        kind: jni::sys::jint,
        text: JString<'caller>,
    ) {
        let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
            if let Some(prompt) = mc_ui::prompt::Prompt::from_code(kind as u8) {
                mc_ui::prompt::answer(prompt, text.try_to_string(env)?);
            }
            Ok(())
        });
        outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
    }

    /// `MainActivity.nativeTakeGithubForget`: true once, after a sign-out, so
    /// the sealed copy on disk goes too.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeTakeGithubForget<'caller>(
        _unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> jni::sys::jboolean {
        mc_ui::git::take_forget_request()
    }

    /// `MainActivity.nativeWorkSummary`: what is running, or "" for nothing.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeWorkSummary<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> jni::sys::jstring {
        let summary = super::work_summary();
        let reference = unowned_env
            .with_env(|env| -> Result<usize, jni::errors::Error> {
                Ok(env.new_string(summary)?.into_raw() as usize)
            })
            .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
        reference as jni::sys::jstring
    }

    /// `MainActivity.nativeComposerMode`: 0 hidden, 1 chat, 2 terminal.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeComposerMode<'caller>(
        _unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> jni::sys::jint {
        mc_ui::safe_area::composer_mode() as jni::sys::jint
    }

    /// `MainActivity.nativeTerminalInput`: raw text for the shell, control
    /// characters included.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeTerminalInput<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        text: JString<'caller>,
    ) {
        let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
            mc_ui::terminal::send_input(text.try_to_string(env)?.into_bytes());
            Ok(())
        });
        outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
    }

    /// `MainActivity.nativeSetSafeArea`, in logical pixels.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeSetSafeArea<'caller>(
        _unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        top: jfloat,
        bottom: jfloat,
    ) {
        mc_ui::safe_area::set(top, bottom);
    }
}
