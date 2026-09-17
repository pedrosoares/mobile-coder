//! The API key, handed down from the Android Keystore.
//!
//! The key never lives in Rust-managed storage and is never written to a file by
//! this code. Kotlin holds it at rest, encrypted by a hardware-backed key in the
//! Android Keystore, and passes the plaintext across JNI once per launch. That
//! keeps the secret out of app data, out of the APK, and out of logs.
//!
//! Set before the agent runs, read many times, so a read-biased lock.

use std::sync::RwLock;

static API_KEY: RwLock<Option<String>> = RwLock::new(None);

/// Non-secret endpoint settings: `(base_url, model)`. Empty strings mean "use the
/// default" (Anthropic's API, `claude-opus-5`).
static ENDPOINT: RwLock<(String, String)> = RwLock::new((String::new(), String::new()));

/// Set where the agent sends requests. Called from Kotlin at startup.
pub fn set_endpoint(base_url: String, model: String) {
    if let Ok(mut slot) = ENDPOINT.write() {
        if !base_url.is_empty() || !model.is_empty() {
            log::info!(
                "endpoint: {} model={}",
                if base_url.is_empty() { "anthropic (default)" } else { &base_url },
                if model.is_empty() { "default" } else { &model },
            );
        }
        *slot = (base_url, model);
    }
}

/// Build the agent config: the stored key plus any endpoint override.
///
/// A key is not required for a custom endpoint - local servers such as LM
/// Studio do not check it - but the header must still be present.
pub fn agent_config() -> Option<mc_agent::AgentConfig> {
    let (base_url, model) = ENDPOINT.read().ok()?.clone();
    let key = get();
    if key.is_none() && base_url.is_empty() {
        return None;
    }
    let mut config = mc_agent::AgentConfig::new(key.unwrap_or_else(|| "local".into()));
    if !base_url.is_empty() {
        config.base_url = mc_agent::messages_url(&base_url);
    }
    if !model.is_empty() {
        config.model = model;
    }
    Some(config)
}

/// Store the key for this process. Called from Kotlin at startup.
pub fn set(key: String) {
    let redacted = redact(&key);
    match API_KEY.write() {
        Ok(mut slot) => {
            *slot = Some(key);
            log::info!("api key installed ({redacted})");
        }
        Err(_) => log::error!("credential lock poisoned; api key not installed"),
    }
}

pub fn get() -> Option<String> {
    API_KEY.read().ok().and_then(|slot| slot.clone())
}

pub fn is_set() -> bool {
    API_KEY.read().map(|slot| slot.is_some()).unwrap_or(false)
}

/// A form safe to log: enough to tell two keys apart, not enough to use one.
fn redact(key: &str) -> String {
    let visible: String = key.chars().take(8).collect();
    format!("{visible}… {} chars", key.len())
}

#[cfg(target_os = "android")]
mod jni_bridge {
    use jni::{
        EnvUnowned,
        objects::{JClass, JString},
    };

    /// `MainActivity.nativeSetEndpoint`.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeSetEndpoint<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        base_url: JString<'caller>,
        model: JString<'caller>,
    ) {
        let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
            super::set_endpoint(base_url.try_to_string(env)?, model.try_to_string(env)?);
            Ok(())
        });
        outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
    }

    /// `MainActivity.nativeSetApiKey`.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeSetApiKey<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        key: JString<'caller>,
    ) {
        let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
            super::set(key.try_to_string(env)?);
            Ok(())
        });
        outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_keeps_a_prefix_and_never_the_whole_key() {
        let key = "sk-ant-api03-SECRETSECRETSECRET";
        let shown = redact(key);
        assert!(shown.starts_with("sk-ant-a"));
        assert!(!shown.contains("SECRET"), "redaction leaked the key: {shown}");
        assert!(shown.contains(&key.len().to_string()));
    }
}
