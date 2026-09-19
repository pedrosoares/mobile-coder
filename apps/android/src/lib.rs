//! Android entry point.
//!
//! Mirrors the structure of Freya's own Android example: a `cdylib` exporting
//! `android_main`, launched by a `NativeActivity` from the Gradle project in
//! `AndroidApp/`.

#[cfg(target_os = "android")]
mod bootstrap;
#[cfg(target_os = "android")]
pub mod agent_task;
#[cfg(target_os = "android")]
pub mod credentials;
#[cfg(target_os = "android")]
pub mod network;

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(droid_app: winit::platform::android::activity::AndroidApp) {
    use freya::{
        android::AndroidPlugin,
        prelude::{LaunchConfig, WindowConfig, launch},
    };
    use freya_winit::renderer::NativeEvent;
    use winit::{event_loop::EventLoop, platform::android::EventLoopBuilderExtAndroid};

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("mobile-coder"),
    );

    // Install the Linux userland off the UI thread. The window should come up
    // immediately; the sandbox becomes available when it is ready.
    if let Some(files_dir) = droid_app.internal_data_path() {
        std::thread::Builder::new()
            .name("mc-bootstrap".into())
            .spawn(move || bootstrap::run(files_dir))
            .expect("failed to spawn the bootstrap thread");
    } else {
        log::error!("no internal data path; cannot install a rootfs");
    }

    let event_loop = EventLoop::<NativeEvent>::with_user_event()
        .with_android_app(droid_app.clone())
        .build()
        .expect("failed to build the event loop");

    // AndroidPlugin provides the AndroidApp as root context and manages the
    // status bar and soft keyboard - the keyboard being driven by whether the
    // focused node has an IME accessibility role.
    // Start the worker before the window, so the chat has a handle on first
    // render. It answers "still installing" until the bootstrap finishes.
    let files_dir = droid_app
        .internal_data_path()
        .unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp"));
    let agent = agent_task::start(files_dir.clone());

    // The conversation as the user left it, rebuilt from the saved transcript.
    let sessions = std::sync::Arc::new(agent_task::library(&files_dir));
    let restored = sessions
        .most_recent()
        .map(|session| mc_core::ChatLog::from_session(&session));

    let mut config = LaunchConfig::new();
    match mc_ui::markdown::android_mono_font() {
        Some(bytes) => config = config.with_font(mc_ui::markdown::MONO_FONT_NAME, bytes),
        None => log::warn!("no system monospace font found; code will use the UI font"),
    }

    launch(
        config
            .with_plugin(AndroidPlugin::new(droid_app))
            .with_window(WindowConfig::new_app(mc_ui::MobileCoder {
                agent: Some(agent),
                // Freya's Input cannot receive on-screen keyboard text in a
                // NativeActivity; MainActivity overlays a native EditText.
                native_composer: true,
                files: Some(std::sync::Arc::new(agent_task::DeviceFiles)),
                shell: Some(std::sync::Arc::new(agent_task::DeviceShell)),
                sandbox: Some(std::sync::Arc::new(agent_task::DeviceSandbox)),
                sessions: Some(sessions),
                restored,
            }))
            .with_event_loop(event_loop),
    )
}
