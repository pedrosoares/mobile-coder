//! The HTTPS client every outbound request goes through.
//!
//! Exists because of an Android-specific trap. `reqwest`'s rustls backend, given
//! no explicit roots, reaches for `rustls-platform-verifier` - which on Android
//! must be handed a JNI environment and an Android `Context` before first use.
//! Miss that and the first HTTPS request panics with *"Expect
//! rustls-platform-verifier to be initialized"*, on a worker thread, long after
//! startup looked healthy.
//!
//! Initialising it is possible but awkward here: `rustls-platform-verifier`
//! builds against `jni` 0.21 while `freya-android` uses 0.22, so the two would
//! have to be threaded through different versions of the same crate.
//!
//! Instead every client is built with an explicit rustls config over the bundled
//! Mozilla root store. That needs no JNI, no Android `Context`, and behaves
//! identically on desktop and device - which also makes TLS failures reproducible
//! off-device.
//!
//! The tradeoff is real and worth stating: user-installed and enterprise CAs on
//! the device are *not* trusted. For talking to `api.anthropic.com` and a distro
//! mirror that is the right call. If this app ever needs to sit behind a
//! corporate TLS-inspecting proxy, this is the thing to revisit.

use std::sync::Arc;

/// Roots and protocol versions, shared by every client.
fn tls_config() -> rustls::ClientConfig {
    // Name the provider rather than relying on a process-wide default: if none
    // is installed, `ClientConfig::builder()` panics at runtime instead of
    // failing to compile.
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };

    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// A client builder with TLS already configured. Use this, never
/// `reqwest::Client::builder()` directly.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_backend_preconfigured(tls_config())
}

/// A ready-to-use client.
pub fn client() -> reqwest::Client {
    builder()
        .build()
        .expect("the TLS configuration is built from constants and cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_root_store_is_not_empty() {
        // An empty store would reject every certificate, and the failure would
        // look like a network problem rather than a configuration one.
        assert!(
            webpki_roots::TLS_SERVER_ROOTS.len() > 50,
            "expected the Mozilla root store, got {} roots",
            webpki_roots::TLS_SERVER_ROOTS.len()
        );
    }

    #[test]
    fn a_client_can_actually_be_built() {
        // Catches a missing crypto provider here rather than on a phone.
        let _ = client();
    }
}
