//! JNI entry point for the execution probe.
//!
//! The probe must run inside the app process to inherit the `untrusted_app`
//! SELinux domain, so it is invoked from an Activity rather than from a shell.
//! See [`probe`] for what it measures and why.

pub mod probe;

#[cfg(target_os = "android")]
mod android {
    use jni::{
        EnvUnowned,
        objects::{JClass, JString},
        sys::jint,
    };

    use crate::probe::{ProbeInput, run};

    /// Called from `MainActivity.runProbe`; returns the rendered report.
    ///
    /// jni 0.22 split `JNIEnv` into `EnvUnowned` (FFI-safe, what the JVM hands
    /// us) and `Env` (the usable API). A native method must upgrade the former
    /// via `with_env` before touching JNI at all - that call also wraps the
    /// closure in `catch_unwind`, so a panic here becomes a Java exception
    /// instead of unwinding across the FFI boundary.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_execprobe_MainActivity_runProbe<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        files_dir: JString<'caller>,
        native_lib_dir: JString<'caller>,
        target_sdk: jint,
    ) -> JString<'caller> {
        let outcome = unowned_env.with_env(|env| -> Result<_, jni::errors::Error> {
            android_logger::init_once(
                android_logger::Config::default()
                    .with_max_level(log::LevelFilter::Debug)
                    .with_tag("mc-exec-probe"),
            );

            let input = ProbeInput {
                files_dir: files_dir.try_to_string(env)?.into(),
                native_lib_dir: native_lib_dir.try_to_string(env)?.into(),
                target_sdk,
            };

            let report = run(&input);
            let text = report.render();

            // Log it as well, so the result survives the activity being killed.
            for line in text.lines() {
                log::info!("{line}");
            }

            // And persist the machine-readable form, so it can be pulled with
            // adb rather than transcribed off a phone screen.
            if let Ok(json) = serde_json::to_string_pretty(&report.to_json()) {
                let _ = std::fs::write(input.files_dir.join("probe-report.json"), json);
            }

            JString::from_str(env, text)
        });

        outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>()
    }
}
