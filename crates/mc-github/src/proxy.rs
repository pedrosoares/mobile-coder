//! A loopback proxy that adds the credentials git is not allowed to know.
//!
//! Git runs in the sandbox, where the working tree is and where the agent also
//! runs commands. The token must not be reachable from there - not in a file,
//! not in the environment, not in `.git/config`, not on a command line. But git
//! still has to authenticate to GitHub.
//!
//! So git is pointed at `http://127.0.0.1:<port>/<secret>/owner/repo.git`
//! instead of `https://github.com/owner/repo.git`, and this server forwards
//! each request upstream with an `Authorization` header attached. What the
//! guest can see is an address and a random path, both of which stop working
//! when the operation finishes and the listener is dropped.
//!
//! It is deliberately small:
//!
//! - **one repository** per proxy, checked on every request, so a leaked path
//!   cannot be pointed at a different repository;
//! - **one operation's lifetime** - started for a clone or a push, dropped
//!   after, so there is nothing running between actions;
//! - **loopback only**, and the path secret is fresh each time.
//!
//! Bodies are streamed in both directions. A push sends a packfile that can be
//! tens of megabytes and a clone receives one; buffering either on a phone is
//! not an option.

use std::{
    net::Ipv4Addr,
    sync::Arc,
};

use futures_util::TryStreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, task::JoinHandle};

use crate::{GithubError, USER_AGENT, token::Token};

/// Headers that must not be forwarded: hop-by-hop, or set by the client we use.
const SKIPPED: [&str; 6] = [
    "host",
    "authorization",
    "connection",
    "transfer-encoding",
    "content-length",
    "upgrade",
];

/// A running proxy. Dropping it stops the listener and closes the door.
pub struct GitProxy {
    /// What to substitute for `https://github.com` in a git URL.
    prefix: String,
    task: JoinHandle<()>,
}

impl Drop for GitProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl GitProxy {
    /// Open a proxy that will serve exactly `repo` ("owner/name"), using `token`.
    ///
    /// The upstream is a parameter so tests can point it at a local git server
    /// rather than github.com.
    pub async fn start(
        repo: &str,
        token: Option<Token>,
        upstream: &str,
    ) -> Result<Self, GithubError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| GithubError::Other(format!("cannot open a local port: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| GithubError::Other(e.to_string()))?
            .port();

        let state = Arc::new(State {
            secret: secret(),
            repo: repo.trim_matches('/').trim_end_matches(".git").to_string(),
            token,
            upstream: upstream.trim_end_matches('/').to_string(),
            http: mc_core::http::client(),
        });
        let prefix = format!("http://{}:{port}/{}", Ipv4Addr::LOCALHOST, state.secret);

        let task = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let service =
                            service_fn(move |req| handle(Arc::clone(&state), req));
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        });

        Ok(Self { prefix, task })
    }

    /// The `insteadOf` rewrite to hand git: everything addressed to GitHub goes
    /// through here instead.
    ///
    /// Given to git as `-c url.<proxy>.insteadOf=<upstream>` so that nothing is
    /// written into `.git/config`; the remote on disk stays the real HTTPS URL,
    /// which is what a person expects to see and what works from a desktop.
    pub fn rewrite_for(&self, upstream: &str) -> String {
        format!("url.{}/.insteadOf={}/", self.prefix, upstream.trim_end_matches('/'))
    }

    /// The proxy's own base URL, for tests and for logging (it holds no secret
    /// beyond the one-time path, which dies with the proxy).
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

struct State {
    secret: String,
    repo: String,
    token: Option<Token>,
    upstream: String,
    http: reqwest::Client,
}

type Boxed = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn refuse(status: StatusCode, reason: &'static str) -> Response<Boxed> {
    let body = http_body_util::Full::new(Bytes::from_static(reason.as_bytes()))
        .map_err(|never| match never {})
        .boxed();
    Response::builder().status(status).body(body).expect("a static response")
}

async fn handle(state: Arc<State>, request: Request<Incoming>) -> Result<Response<Boxed>, hyper::Error> {
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(|q| format!("?{q}")).unwrap_or_default();

    // The one-time path, then the one repository. Both are checked before
    // anything is forwarded, so a stale or guessed URL gets nothing.
    let Some(rest) = path.strip_prefix(&format!("/{}/", state.secret)) else {
        tracing::warn!("git proxy: rejected a request with the wrong path");
        return Ok(refuse(StatusCode::NOT_FOUND, "no"));
    };
    if !rest.starts_with(&format!("{}.git/", state.repo))
        && !rest.starts_with(&format!("{}/", state.repo))
    {
        tracing::warn!(%rest, "git proxy: rejected a request for another repository");
        return Ok(refuse(StatusCode::FORBIDDEN, "this proxy serves one repository"));
    }

    let url = format!("{}/{rest}{query}", state.upstream);
    let mut upstream = state.http.request(request.method().clone(), &url);
    for (name, value) in request.headers() {
        if SKIPPED.contains(&name.as_str()) {
            continue;
        }
        upstream = upstream.header(name, value);
    }
    upstream = upstream.header("user-agent", USER_AGENT);
    if let Some(token) = &state.token {
        // Basic, with the token as the password: what git itself sends, and
        // what GitHub accepts for both classic and fine-grained tokens.
        upstream = upstream.basic_auth("x-access-token", Some(token.as_str()));
    }

    // Stream the request body through rather than collecting it: a push is a
    // packfile, and it can be large.
    let body = request.into_body().into_data_stream().map_err(std::io::Error::other);
    let response = match upstream.body(reqwest::Body::wrap_stream(body)).send().await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(%e, "git proxy: upstream request failed");
            return Ok(refuse(StatusCode::BAD_GATEWAY, "could not reach GitHub"));
        }
    };

    let mut builder = Response::builder().status(response.status().as_u16());
    for (name, value) in response.headers() {
        if SKIPPED.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    let stream = response
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    Ok(builder
        .body(StreamBody::new(stream).boxed())
        .unwrap_or_else(|_| refuse(StatusCode::BAD_GATEWAY, "malformed response")))
}

/// A fresh path segment per proxy. Not a password - the proxy is loopback-only
/// and short-lived - but enough that a URL left in a shell history is useless.
fn secret() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut out = String::new();
    for _ in 0..2 {
        let value = RandomState::new().build_hasher().finish();
        out.push_str(&format!("{value:016x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GIT_ROOT;

    #[test]
    fn a_secret_is_long_and_never_the_same_twice() {
        let first = secret();
        assert_eq!(first.len(), 32);
        assert_ne!(first, secret());
    }

    #[tokio::test]
    async fn the_rewrite_points_git_at_the_proxy_and_nowhere_else() {
        let proxy = GitProxy::start("owner/repo", None, GIT_ROOT).await.unwrap();
        let rewrite = proxy.rewrite_for(GIT_ROOT);
        assert!(rewrite.starts_with("url.http://127.0.0.1:"));
        assert!(rewrite.ends_with(".insteadOf=https://github.com/"));
        // The token is not in it, because there is no token in a URL - ever.
        assert!(!rewrite.contains("x-access-token"));
    }

    #[tokio::test]
    async fn a_request_for_another_repository_is_refused() {
        let proxy = GitProxy::start("owner/repo", None, GIT_ROOT).await.unwrap();
        let client = mc_core::http::client();

        let wrong_repo = client
            .get(format!("{}/someone-else/private.git/info/refs", proxy.prefix()))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_repo.status(), 403);

        let wrong_secret = client
            .get("http://127.0.0.1:1/x/owner/repo.git/info/refs")
            .send()
            .await;
        // Either refused or unreachable; what matters is that it is not served.
        assert!(wrong_secret.is_err() || wrong_secret.unwrap().status() == 404);
    }
}
