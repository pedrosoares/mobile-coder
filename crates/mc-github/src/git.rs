//! Git, run where the working tree is.
//!
//! The repository lives inside the sandbox, so git runs inside the sandbox too:
//! the same `git` the user sees in the Terminal tab, and the same one the agent
//! can reach with its own tools. What does *not* go in there is the token -
//! anything needing credentials goes through [`crate::proxy`].
//!
//! Local work (status, commit) needs no credentials at all and is just a
//! command. Network work (clone, pull, push) takes a proxy that lives for the
//! length of the operation.

use std::time::Duration;

use mc_sandbox::Sandbox;

use crate::{GIT_ROOT, GithubError, proxy::GitProxy, token::Token};

/// Long enough for a real clone on a phone's connection, short enough that a
/// hung transfer does not become a permanently stuck screen.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(600);
/// Local git is fast or broken.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Where clones land when nothing says otherwise: a guest path, since the
/// phone is the target. The desktop dev loop passes its workspace instead - see
/// [`mc_sandbox::SandboxFactory::projects_dir`].
pub const PROJECTS_DIR: &str = "/root/projects";

/// What the repository in a directory looks like right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub branch: String,
    /// Paths with changes, staged or not, including untracked files.
    pub changed: Vec<String>,
    /// Commits this branch has that its upstream does not.
    pub ahead: u32,
    /// Commits the upstream has that this branch does not.
    pub behind: u32,
    /// `owner/name` when the remote is a GitHub repository.
    pub repo: Option<String>,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.changed.is_empty()
    }

    /// One line for a phone screen: `main · 3 changes · 1 ahead`.
    pub fn summary(&self) -> String {
        let mut parts = vec![if self.branch.is_empty() { "(no branch)".into() } else { self.branch.clone() }];
        match self.changed.len() {
            0 => parts.push("clean".into()),
            1 => parts.push("1 change".into()),
            n => parts.push(format!("{n} changes")),
        }
        if self.ahead > 0 {
            parts.push(format!("{} ahead", self.ahead));
        }
        if self.behind > 0 {
            parts.push(format!("{} behind", self.behind));
        }
        parts.join(" · ")
    }
}

/// Settings every git command in the sandbox needs.
///
/// `core.createObject=rename` is not a preference, it is the difference between
/// a repository that works and one that silently is not a repository. Git
/// writes a loose object by writing a temporary file, `link()`ing it into place
/// and unlinking the temporary. Android forbids hard links, so proot emulates
/// `link()` (`--link2symlink`) - and the emulation leaves the object somewhere
/// git does not look. Measured on the Fold6: after a commit, `.git/objects/71/`
/// held two `.l2s.tmp_obj_…` files and no object, `refs/heads/main` pointed at a
/// commit that did not exist, and every later command died with
/// `fatal: bad object HEAD`. `rename` is git's own switch for filesystems
/// without working hard links, and it makes git use `rename()` instead.
const GIT_SETTINGS: [&str; 2] = ["-c", "core.createObject=rename"];

/// Run git in `dir` with the given arguments, as one shell command.
///
/// Arguments are quoted here rather than by the caller, because every one of
/// them - a branch name, a commit message, a URL - is either user- or
/// model-supplied, and an unquoted one is a command.
async fn git(
    sandbox: &Sandbox,
    dir: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, GithubError> {
    let command = std::iter::once("git".to_string())
        .chain(GIT_SETTINGS.iter().chain(args.iter()).map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    let command = format!("cd {} && {command}", shell_quote(dir));

    let output = sandbox
        .run_with_timeout(&command, None, Some(timeout))
        .await
        .map_err(|e| GithubError::Other(e.to_string()))?;

    if output.timed_out {
        return Err(GithubError::Other(format!(
            "git took longer than {}s and was stopped",
            timeout.as_secs()
        )));
    }
    if !output.ok() {
        // git says what went wrong on stderr, and it is usually the most
        // useful sentence available - pass it through rather than paraphrase.
        let reason = output.stderr.trim();
        let reason = if reason.is_empty() { output.stdout.trim() } else { reason };
        // Logged as well as returned. A screen shows one line; when something
        // is wrong the command that produced it is what settles the argument.
        tracing::warn!(status = ?output.status, %command, "git failed: {reason}");
        return Err(GithubError::Other(clip(reason, 400)));
    }
    tracing::debug!(%command, "git ok");
    Ok(output.stdout)
}

/// Single-quote for `/bin/sh`.
fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// Read the state of the repository in `dir`.
pub async fn status(sandbox: &Sandbox, dir: &str) -> Result<Status, GithubError> {
    let porcelain = git(
        sandbox,
        dir,
        // v2 gives the branch, its upstream and the ahead/behind counts in the
        // same call, so a phone makes one round trip instead of four.
        &["status", "--porcelain=v2", "--branch", "--untracked-files=normal"],
        LOCAL_TIMEOUT,
    )
    .await?;
    let remote = git(sandbox, dir, &["remote", "get-url", "origin"], LOCAL_TIMEOUT)
        .await
        .unwrap_or_default();

    let mut status = parse_status(&porcelain);
    status.repo = repo_of(remote.trim());
    Ok(status)
}

/// Parse `git status --porcelain=v2 --branch`.
fn parse_status(porcelain: &str) -> Status {
    let mut status = Status::default();
    for line in porcelain.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            status.branch = head.trim().to_string();
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            // "+1 -2": ahead of upstream by one, behind by two.
            for part in ab.split_whitespace() {
                let value = part[1..].parse().unwrap_or(0);
                match part.chars().next() {
                    Some('+') => status.ahead = value,
                    Some('-') => status.behind = value,
                    _ => {}
                }
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            status.changed.push(path.trim().to_string());
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            // Ordinary and renamed entries: the path is the last field, and a
            // rename adds a tab-separated original which is not wanted here.
            if let Some(path) = line.split_whitespace().nth(8) {
                status.changed.push(path.split('\t').next().unwrap_or(path).to_string());
            }
        }
    }
    status
}

/// `owner/name` from a GitHub remote URL, in any of the forms git accepts.
pub fn repo_of(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

/// Stage everything and commit.
///
/// No credentials, no network: a commit is local, which is why the agent is
/// allowed to make one and not allowed to push it.
pub async fn commit(
    sandbox: &Sandbox,
    dir: &str,
    message: &str,
    author: Option<&(String, String)>,
) -> Result<String, GithubError> {
    if message.trim().is_empty() {
        return Err(GithubError::Other("a commit needs a message".into()));
    }
    git(sandbox, dir, &["add", "-A"], LOCAL_TIMEOUT).await?;

    let mut args: Vec<&str> = Vec::new();
    // Identity per invocation rather than written into the repository: the
    // account can change, and a stale one in `.git/config` is worse than none.
    let identity;
    if let Some((name, email)) = author {
        identity = [format!("user.name={name}"), format!("user.email={email}")];
        args.extend(["-c", &identity[0], "-c", &identity[1]]);
    }
    args.extend(["commit", "-m", message]);
    git(sandbox, dir, &args, LOCAL_TIMEOUT).await
}

/// Clone `repo` ("owner/name") into [`PROJECTS_DIR`], shallow by default.
///
/// Returns the directory it landed in. History is usually the largest part of a
/// repository and a phone has the least room for it, so `depth` defaults to one
/// commit; the caller can ask for everything when the history is the point.
pub async fn clone(
    sandbox: &Sandbox,
    projects_dir: &str,
    repo: &str,
    token: Option<Token>,
    depth: Option<u32>,
) -> Result<String, GithubError> {
    let name = repo.rsplit('/').next().unwrap_or(repo).trim_end_matches(".git");
    let dir = format!("{projects_dir}/{name}");

    let proxy = GitProxy::start(repo, token, GIT_ROOT).await?;
    let rewrite = proxy.rewrite_for(GIT_ROOT);
    let url = format!("{GIT_ROOT}/{}.git", repo.trim_end_matches(".git"));

    let depth_arg = depth.map(|d| d.to_string());
    let mut args = vec!["-c", &rewrite, "clone"];
    if let Some(depth) = &depth_arg {
        args.extend(["--depth", depth]);
    }
    args.extend([url.as_str(), dir.as_str()]);

    // git creates the last component of the path but not the rest.
    sandbox
        .run_with_timeout(&format!("mkdir -p {}", shell_quote(projects_dir)), None, Some(LOCAL_TIMEOUT))
        .await
        .map_err(|e| GithubError::Other(e.to_string()))?;

    git(sandbox, projects_dir, &args, NETWORK_TIMEOUT).await?;
    Ok(dir)
}

/// Push the current branch to `origin`.
///
/// Always a user's action, never the agent's: it publishes code, and a model
/// that read a repository's files is not the right thing to be deciding that.
pub async fn push(
    sandbox: &Sandbox,
    dir: &str,
    token: Option<Token>,
) -> Result<String, GithubError> {
    let status = status(sandbox, dir).await?;
    let repo = status
        .repo
        .ok_or_else(|| GithubError::Other("this project has no GitHub remote".into()))?;

    let proxy = GitProxy::start(&repo, token, GIT_ROOT).await?;
    let rewrite = proxy.rewrite_for(GIT_ROOT);
    // `-u` so the branch has an upstream afterwards and the ahead/behind counts
    // mean something on the next status.
    git(
        sandbox,
        dir,
        &["-c", &rewrite, "push", "-u", "origin", "HEAD"],
        NETWORK_TIMEOUT,
    )
    .await
    .map_err(|e| explain_push(e, &repo))
}

/// Turn git's push failures into something a person can act on.
///
/// The common one on a phone is a token that can read but not write: the
/// repository list fills in, the clone works, and then the push is refused.
/// git's own words for that mention the loopback proxy's URL, which is an
/// implementation detail and reads like a different problem entirely.
fn explain_push(error: GithubError, repo: &str) -> GithubError {
    let GithubError::Other(reason) = &error else { return error };
    let lower = reason.to_ascii_lowercase();

    if lower.contains("denied to") || lower.contains("403") {
        return GithubError::Other(format!(
            "The token cannot write to {repo}. On github.com, give it \
             Contents: Read and write, and make sure {repo} is one of the repositories it can \
             reach."
        ));
    }
    if lower.contains("non-fast-forward") || lower.contains("rejected") {
        return GithubError::Other(
            "The remote has commits this branch does not. Pull first, then push again.".into(),
        );
    }
    error
}

/// Fetch and fast-forward. Refuses to merge: resolving a conflict on a phone is
/// not something this app can offer honestly, so a diverged branch stops here
/// and says so.
pub async fn pull(
    sandbox: &Sandbox,
    dir: &str,
    token: Option<Token>,
) -> Result<String, GithubError> {
    let status = status(sandbox, dir).await?;
    let repo = status
        .repo
        .ok_or_else(|| GithubError::Other("this project has no GitHub remote".into()))?;

    let proxy = GitProxy::start(&repo, token, GIT_ROOT).await?;
    let rewrite = proxy.rewrite_for(GIT_ROOT);
    git(sandbox, dir, &["-c", &rewrite, "fetch", "origin"], NETWORK_TIMEOUT).await?;
    git(sandbox, dir, &["merge", "--ff-only", "@{u}"], LOCAL_TIMEOUT)
        .await
        .map_err(|e| match e {
            GithubError::Other(reason) if reason.contains("Not possible to fast-forward") => {
                GithubError::Other(
                    "the branch and its remote have both moved on. Sort it out where there is a \
                     bigger screen, or reset to the remote and lose the local commits."
                        .into(),
                )
            }
            other => other,
        })
}

/// Set the remote of an existing project, for a repository created from here.
pub async fn set_origin(sandbox: &Sandbox, dir: &str, repo: &str) -> Result<(), GithubError> {
    let url = format!("{GIT_ROOT}/{}.git", repo.trim_end_matches(".git"));
    // `set-url` fails when there is no origin yet, so try adding first.
    if git(sandbox, dir, &["remote", "add", "origin", &url], LOCAL_TIMEOUT).await.is_err() {
        git(sandbox, dir, &["remote", "set-url", "origin", &url], LOCAL_TIMEOUT).await?;
    }
    Ok(())
}

/// Start a repository in `dir` if it is not one already.
pub async fn init(sandbox: &Sandbox, dir: &str) -> Result<(), GithubError> {
    git(sandbox, dir, &["init", "-b", "main"], LOCAL_TIMEOUT).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point, end to end: git speaking to GitHub through the proxy,
    /// with no credentials anywhere git can see. Needs the network, so it is
    /// not part of the normal run:
    ///
    /// ```sh
    /// cargo test -p mc-github clone_through_the_proxy -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn clone_through_the_proxy_reaches_a_real_repository() {
        let dir = std::env::temp_dir().join(format!("mc-proxy-clone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sandbox = Sandbox::host();

        // A public repository, so this proves the transport without needing a
        // token - the header is the only thing a token would add.
        let proxy = GitProxy::start("octocat/Hello-World", None, GIT_ROOT).await.unwrap();
        let rewrite = proxy.rewrite_for(GIT_ROOT);
        let target = dir.display().to_string();

        let out = git(
            &sandbox,
            "/tmp",
            &[
                "-c",
                &rewrite,
                "clone",
                "--depth",
                "1",
                "https://github.com/octocat/Hello-World.git",
                &target,
            ],
            NETWORK_TIMEOUT,
        )
        .await;
        assert!(out.is_ok(), "clone failed: {out:?}");
        let _ = &proxy; // held open for the whole clone, and closed after it
        assert!(dir.join("README").exists(), "the working tree is there");

        // What was written to disk is the real URL, not the proxy: the proxy is
        // gone in a moment, and a config pointing at a dead port would be worse
        // than useless.
        let config = std::fs::read_to_string(dir.join(".git/config")).unwrap();
        assert!(config.contains("https://github.com/octocat/Hello-World"), "{config}");
        assert!(!config.contains("127.0.0.1"), "the proxy leaked into the config: {config}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one setting a repository in this sandbox cannot do without.
    #[tokio::test]
    async fn every_git_command_carries_the_hard_link_workaround() {
        // Asked of a real shell, so the quoting is proven too: `git` prints
        // what it was configured with.
        let out = git(
            &Sandbox::host(),
            "/tmp",
            &["config", "--get", "core.createObject"],
            LOCAL_TIMEOUT,
        )
        .await
        .expect("git should answer");
        assert_eq!(out.trim(), "rename");
    }

    #[test]
    fn a_refused_push_says_what_to_change_rather_than_what_git_said() {
        // Real output, proxy URL and all.
        let raw = GithubError::Other(
            "remote: Permission to pedrosoares/mobile-coder-page.git denied to pedrosoares.\n\
             fatal: unable to access 'http://127.0.0.1:42689/abc/pedrosoares/mobile-coder-page.git/': \
             The requested URL returned error: 403"
                .into(),
        );
        let said = explain_push(raw, "pedrosoares/mobile-coder-page").to_string();
        assert!(said.contains("Contents: Read and write"), "says what to change: {said}");
        assert!(!said.contains("127.0.0.1"), "the proxy is not the user's problem: {said}");
    }

    #[test]
    fn a_push_behind_the_remote_is_told_to_pull() {
        let raw = GithubError::Other(
            "! [rejected] main -> main (non-fast-forward)".into(),
        );
        assert!(explain_push(raw, "o/r").to_string().contains("Pull first"));
    }

    #[test]
    fn anything_else_is_passed_through_as_git_said_it() {
        let raw = GithubError::Other("error: src refspec main does not match any".into());
        assert_eq!(
            explain_push(raw, "o/r").to_string(),
            "error: src refspec main does not match any",
        );
    }

    #[test]
    fn a_remote_url_reduces_to_owner_and_name_in_any_form_git_accepts() {
        let expected = Some("pedrosoares/mobile-coder".to_string());
        assert_eq!(repo_of("https://github.com/pedrosoares/mobile-coder.git"), expected);
        assert_eq!(repo_of("https://github.com/pedrosoares/mobile-coder"), expected);
        assert_eq!(repo_of("git@github.com:pedrosoares/mobile-coder.git"), expected);
        assert_eq!(repo_of("ssh://git@github.com/pedrosoares/mobile-coder.git"), expected);
        // Not GitHub, or not a repository.
        assert_eq!(repo_of("https://gitlab.com/someone/thing.git"), None);
        assert_eq!(repo_of("https://github.com/pedrosoares"), None);
        assert_eq!(repo_of(""), None);
    }

    #[test]
    fn status_reads_the_branch_its_drift_and_what_changed() {
        // Real `--porcelain=v2 --branch` output, trimmed.
        let porcelain = "\
# branch.oid 0d6d1ee2f6e2f1b0f1a
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
1 .M N... 100644 100644 100644 abc def crates/mc-ui/src/chat.rs
1 A. N... 000000 100644 100644 000000 abc crates/mc-github/src/git.rs
? notes.txt
";
        let status = parse_status(porcelain);
        assert_eq!(status.branch, "main");
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
        assert_eq!(
            status.changed,
            ["crates/mc-ui/src/chat.rs", "crates/mc-github/src/git.rs", "notes.txt"]
        );
        assert!(!status.is_clean());
        assert_eq!(status.summary(), "main · 3 changes · 2 ahead · 1 behind");
    }

    #[test]
    fn a_clean_repository_says_so_in_one_line() {
        let status = parse_status("# branch.head main\n# branch.ab +0 -0\n");
        assert!(status.is_clean());
        assert_eq!(status.summary(), "main · clean");
    }

    #[test]
    fn a_detached_head_does_not_pretend_to_be_a_branch() {
        let status = parse_status("# branch.head (detached)\n");
        assert_eq!(status.summary(), "(detached) · clean");
    }

    /// Branch names, commit messages and URLs all reach the shell, and all of
    /// them come from a person or a model. Prove the quoting against a real
    /// shell rather than against an assumption about one.
    #[tokio::test]
    async fn an_argument_with_a_quote_in_it_cannot_become_a_command() {
        let marker = std::env::temp_dir().join(format!("mc-quoting-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let hostile = format!("'; touch {}; echo '", marker.display());

        let out = Sandbox::host()
            .run(&format!("printf %s {}", shell_quote(&hostile)), None)
            .await
            .unwrap();

        assert_eq!(out.stdout, hostile, "it came back as one literal argument");
        assert!(!marker.exists(), "the injected command ran");
    }
}
