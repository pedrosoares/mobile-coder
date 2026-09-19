//! The GitHub token, for as long as the app runs.
//!
//! Held in memory only. On Android the durable copy is sealed by the Keystore
//! (`KeyVault`) and handed over at launch; on desktop it comes from
//! `GITHUB_TOKEN`. Either way this module is the only place the plaintext
//! lives, and nothing here writes it anywhere.

use std::sync::RwLock;

static TOKEN: RwLock<Option<String>> = RwLock::new(None);

/// A token, with a `Debug` that cannot leak it into a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A form safe to log or show: enough to tell two tokens apart, not enough
    /// to use one. GitHub's own prefixes (`ghp_`, `github_pat_`) survive, which
    /// is what makes it useful for telling a user which kind they pasted.
    pub fn redacted(&self) -> String {
        let visible: String = self.0.chars().take(8).collect();
        format!("{visible}… {} chars", self.0.len())
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token({})", self.redacted())
    }
}

/// Store the token for this process.
pub fn set(token: Token) {
    let redacted = token.redacted();
    match TOKEN.write() {
        Ok(mut slot) => {
            *slot = Some(token.0);
            tracing::info!("github token installed ({redacted})");
        }
        Err(_) => tracing::error!("github token lock poisoned; token not installed"),
    }
}

pub fn get() -> Option<Token> {
    TOKEN.read().ok().and_then(|slot| slot.clone().map(Token))
}

pub fn is_set() -> bool {
    TOKEN.read().map(|slot| slot.is_some()).unwrap_or(false)
}

/// Forget it. Used by "Sign out", and by the app when the token is rejected.
pub fn clear() {
    if let Ok(mut slot) = TOKEN.write() {
        *slot = None;
        tracing::info!("github token cleared");
    }
}

/// The token from the environment, for the desktop dev loop.
pub fn from_env() -> Option<Token> {
    std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| Token::new(value.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_never_prints_itself() {
        let token = Token::new("github_pat_11ABCDEFG0secretsecretsecret");
        let redacted = token.redacted();
        assert!(redacted.starts_with("github_p"), "enough to recognise: {redacted}");
        assert!(!redacted.contains("secret"), "leaked: {redacted}");
        // Debug is where secrets escape - a struct printed in an error, a
        // tracing field - so it goes through the same redaction.
        assert!(!format!("{token:?}").contains("secret"));
    }
}
