//! GitHub, from a phone.
//!
//! Two jobs, kept apart on purpose:
//!
//! - the **REST API** (who am I, what repositories are there, make a new one),
//!   which is plain HTTPS from this process;
//! - **git itself** - clone, commit, push - which runs in the sandbox, where
//!   the working tree lives, through [`git`].
//!
//! # Where the token lives, and why that took some thinking
//!
//! The token can publish code and, for a fine-grained token with write access,
//! change it. The agent runs arbitrary commands in the same sandbox the working
//! tree is in, so anything reachable from there - a file, an environment
//! variable, a remote URL in `.git/config`, a process's command line - is
//! reachable by a model following instructions it read in a cloned repository.
//!
//! So the token stays in this process and never enters the guest. Git still
//! needs to authenticate, though, and git runs in the guest. That is what
//! [`proxy`] is for: a loopback HTTP server, opened for one operation and shut
//! afterwards, which forwards git's requests to GitHub over HTTPS and adds the
//! credentials on the way past. The guest talks plain HTTP to `127.0.0.1` and
//! never sees a secret; what it can see - the proxy's address and its one-time
//! path - stops working the moment the operation ends.
//!
//! `docs/ARCHITECTURE.md` §9 records why this replaced the original plan of
//! linking libgit2 into the app.

pub mod git;
pub mod proxy;
pub mod token;

use serde::Deserialize;

pub use token::Token;

/// GitHub's API root. Overridable so tests can point at a local server.
pub const API_ROOT: &str = "https://api.github.com";
/// Where git fetches and pushes. Also overridable for tests.
pub const GIT_ROOT: &str = "https://github.com";

#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("network request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The token is missing, expired, or lacks the scope for this call.
    #[error("GitHub rejected the token ({0})")]
    Unauthorized(String),
    #[error("GitHub returned {status}: {message}")]
    Api { status: u16, message: String },
    #[error("{0}")]
    Other(String),
}

/// Who the token belongs to.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Viewer {
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// A repository, reduced to what a phone screen can show and act on.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Repo {
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
    pub clone_url: String,
    #[serde(default)]
    pub default_branch: String,
    /// ISO 8601, straight from the API. Sorting is done by the server.
    #[serde(default)]
    pub updated_at: String,
}

impl Repo {
    /// The part after the owner, which is what a narrow screen has room for.
    pub fn name(&self) -> &str {
        self.full_name.rsplit('/').next().unwrap_or(&self.full_name)
    }
}

/// A client for the bits of GitHub's API this app uses.
#[derive(Debug, Clone)]
pub struct Api {
    http: reqwest::Client,
    token: String,
    api_root: String,
}

impl Api {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            // The shared client: bundled roots, because Android's platform
            // verifier panics under this stack (see `mc_core::http`).
            http: mc_core::http::client(),
            token: token.into(),
            api_root: API_ROOT.to_string(),
        }
    }

    /// Point at a different API root - a test server, or GitHub Enterprise.
    pub fn with_api_root(mut self, root: impl Into<String>) -> Self {
        self.api_root = root.into().trim_end_matches('/').to_string();
        self
    }

    /// Who the token belongs to. Doubles as "is this token any good?".
    pub async fn viewer(&self) -> Result<Viewer, GithubError> {
        self.get("/user").await
    }

    /// Repositories the user can push to, most recently updated first.
    ///
    /// `affiliation` includes repositories owned by organisations, which is
    /// where work repositories usually live.
    pub async fn repos(&self) -> Result<Vec<Repo>, GithubError> {
        self.get("/user/repos?per_page=50&sort=updated&affiliation=owner,collaborator,organization_member")
            .await
    }

    /// Create a repository under the user's own account.
    ///
    /// `auto_init` gives it a first commit, which matters on a phone: pushing
    /// into a repository with no commits at all is the one case where git needs
    /// extra explaining, and there is no room on screen to explain it.
    pub async fn create_repo(
        &self,
        name: &str,
        private: bool,
        auto_init: bool,
    ) -> Result<Repo, GithubError> {
        let body = serde_json::json!({
            "name": name,
            "private": private,
            "auto_init": auto_init,
        });
        let response = self
            .request(reqwest::Method::POST, "/user/repos")
            .json(&body)
            .send()
            .await?;
        self.decode(response).await
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.api_root))
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", USER_AGENT)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, GithubError> {
        let response = self.request(reqwest::Method::GET, path).send().await?;
        self.decode(response).await
    }

    /// Turn a response into a value, or into an error a person can act on.
    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, GithubError> {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return serde_json::from_str(&body)
                .map_err(|e| GithubError::Other(format!("GitHub sent something unexpected: {e}")));
        }

        // GitHub puts the useful part in `message`, and often a second line in
        // `errors`; the raw JSON body is not worth showing on a phone.
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
            .unwrap_or_else(|| body.chars().take(200).collect());

        match status.as_u16() {
            401 | 403 => Err(GithubError::Unauthorized(message)),
            status => Err(GithubError::Api { status, message }),
        }
    }
}

/// Sent with every request. GitHub asks for one and rejects anonymous agents.
pub const USER_AGENT: &str = "mobile-coder";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_shows_its_short_name_on_a_narrow_screen() {
        let repo = Repo {
            full_name: "pedrosoares/mobile-coder".into(),
            private: true,
            clone_url: "https://github.com/pedrosoares/mobile-coder.git".into(),
            default_branch: "main".into(),
            updated_at: String::new(),
        };
        assert_eq!(repo.name(), "mobile-coder");
    }

    #[test]
    fn repositories_decode_from_what_the_api_actually_sends() {
        // A trimmed real response: the fields this app uses, among many it
        // ignores, which is why every one of them is `default`ed or required
        // deliberately.
        let json = serde_json::json!([{
            "id": 1,
            "full_name": "octocat/Hello-World",
            "private": false,
            "clone_url": "https://github.com/octocat/Hello-World.git",
            "default_branch": "master",
            "updated_at": "2026-09-18T12:00:00Z",
            "owner": { "login": "octocat" }
        }]);
        let repos: Vec<Repo> = serde_json::from_value(json).unwrap();
        assert_eq!(repos[0].name(), "Hello-World");
        assert_eq!(repos[0].default_branch, "master");
    }
}
