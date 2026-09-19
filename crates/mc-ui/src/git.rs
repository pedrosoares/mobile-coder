//! The Git pane: the account, the projects, and the four buttons that matter.
//!
//! Everything here happens on a worker thread with its own tokio runtime, for
//! the same reason the agent does: a clone is a network operation measured in
//! seconds, and the UI thread is drawing. The pane holds no logic of its own -
//! it sends a [`Command`], polls [`state`], and draws whatever it finds.
//!
//! Why a pane at all, rather than settings: on a phone the repository *is* the
//! storage story. A project that is pushed can be deleted and cloned back; one
//! that is not is stuck on the device forever. So this screen is about the state
//! of the work - what has changed, what is ahead of the remote - and not only
//! about credentials.

use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use freya::prelude::*;
use mc_github::{Api, GithubError, Repo, git, token::Token};
use mc_sandbox::SandboxFactory;

use crate::{
    prompt::{self, Prompt},
    theme,
};

/// How often the pane looks for work the worker has finished.
const POLL: Duration = Duration::from_millis(300);

/// What the pane can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A token the user just typed, or one restored at launch.
    SignIn(String),
    SignOut,
    /// Re-read everything: the account, the repository list, the projects.
    Refresh,
    Clone(String),
    /// Create a repository under the user's account, then clone it.
    Create(String),
    Select(String),
    Commit(String),
    Push,
    Pull,
}

/// A project on the device: a git repository under [`git::PROJECTS_DIR`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub dir: String,
    pub name: String,
    /// One line: branch, changes, ahead/behind. `None` until it has been read.
    pub status: Option<String>,
    pub clean: bool,
}

/// Everything the pane draws. Replaced wholesale by the worker, so the pane
/// never has to merge partial updates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitState {
    pub signed_in: Option<String>,
    pub repos: Vec<RepoRow>,
    pub projects: Vec<Project>,
    pub selected: Option<String>,
    /// What is running, in words: "Cloning octocat/Hello-World…".
    pub busy: Option<String>,
    /// The outcome of the last thing that finished, good or bad.
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRow {
    pub full_name: String,
    pub private: bool,
    /// Already on the device, so the button says Open rather than Clone.
    pub cloned: bool,
}

static STATE: Mutex<Option<GitState>> = Mutex::new(None);
static VERSION: AtomicU64 = AtomicU64::new(0);
static COMMANDS: OnceLock<std::sync::mpsc::Sender<Command>> = OnceLock::new();
/// Raised when the user signs out, so the shell can forget the stored token.
static FORGET: AtomicBool = AtomicBool::new(false);

/// The current state, and a version that changes whenever it does.
pub fn state() -> (u64, GitState) {
    let version = VERSION.load(Ordering::Relaxed);
    let state = STATE.lock().ok().and_then(|s| s.clone()).unwrap_or_default();
    (version, state)
}

fn publish(state: GitState) {
    if let Ok(mut slot) = STATE.lock() {
        *slot = Some(state);
    }
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// Send a command, if the worker is running.
pub fn send(command: Command) {
    if let Some(tx) = COMMANDS.get() {
        let _ = tx.send(command);
    }
}

/// True once after the user signs out: the durable copy of the token is the
/// shell's to delete, and only Android has one.
pub fn take_forget_request() -> bool {
    FORGET.swap(false, Ordering::Relaxed)
}

/// Start the worker. Called once, from the app root.
///
/// The token is *not* a parameter. On Android it arrives from the Keystore on
/// the UI thread, which races the native side starting up - passing it in meant
/// whichever lost the race decided whether the app opened signed in (measured
/// on the Fold6: the token was restored, the pane said "Not signed in").
/// Instead the first command is a refresh, which reads whatever
/// [`mc_github::token`] holds by then, and setting a token later sends another.
pub fn start(factory: Arc<dyn SandboxFactory>) {
    let (tx, rx) = std::sync::mpsc::channel();
    if COMMANDS.set(tx).is_err() {
        return; // already running
    }

    std::thread::Builder::new()
        .name("mc-git".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    log::error!("no runtime for git; the Git pane will not work: {e}");
                    return;
                }
            };
            let mut worker = Worker { factory, state: GitState::default() };
            runtime.block_on(async move {
                worker.run(Command::Refresh).await;
                while let Ok(command) = rx.recv() {
                    worker.run(command).await;
                }
            });
        })
        .ok();
}

struct Worker {
    factory: Arc<dyn SandboxFactory>,
    state: GitState,
}

impl Worker {
    async fn run(&mut self, command: Command) {
        self.state.busy = Some(describe(&command));
        self.state.message = None;
        publish(self.state.clone());

        let outcome = self.execute(command).await;
        self.state.busy = None;
        self.state.message = match outcome {
            Ok(Some(message)) => Some(message),
            Ok(None) => None,
            Err(e) => Some(say(e)),
        };
        publish(self.state.clone());
    }

    async fn execute(&mut self, command: Command) -> Result<Option<String>, GithubError> {
        match command {
            Command::SignIn(token) => {
                mc_github::token::set(Token::new(token));
                self.load_account().await?;
                self.load_projects().await;
                Ok(Some(match &self.state.signed_in {
                    Some(login) => format!("Signed in as {login}."),
                    None => "Signed in.".into(),
                }))
            }

            Command::SignOut => {
                mc_github::token::clear();
                FORGET.store(true, Ordering::Relaxed);
                self.state.signed_in = None;
                self.state.repos.clear();
                Ok(Some("Signed out. The token was deleted from this device.".into()))
            }

            Command::Refresh => {
                if mc_github::token::is_set() {
                    self.load_account().await?;
                }
                self.load_projects().await;
                Ok(None)
            }

            Command::Clone(full_name) => {
                let sandbox = self.sandbox()?;
                let dir = git::clone(
                    &sandbox,
                    &self.factory.projects_dir(),
                    &full_name,
                    mc_github::token::get(),
                    Some(1),
                )
                .await?;
                self.state.selected = Some(dir.clone());
                self.load_projects().await;
                Ok(Some(format!("Cloned into {dir}.")))
            }

            Command::Create(name) => {
                let api = self.api()?;
                // Private and with a first commit: the safe default for
                // something made on a phone, and the shape that clones cleanly.
                let repo = api.create_repo(&name, true, true).await?;
                let sandbox = self.sandbox()?;
                let dir = git::clone(
                    &sandbox,
                    &self.factory.projects_dir(),
                    &repo.full_name,
                    mc_github::token::get(),
                    Some(1),
                )
                .await?;
                self.state.selected = Some(dir.clone());
                self.load_account().await?;
                self.load_projects().await;
                Ok(Some(format!("Created {} and cloned it.", repo.full_name)))
            }

            Command::Select(dir) => {
                self.state.selected = Some(dir);
                self.load_projects().await;
                Ok(None)
            }

            Command::Commit(message) => {
                let (sandbox, dir) = self.project()?;
                let author = self.state.signed_in.as_ref().map(|login| {
                    (login.clone(), format!("{login}@users.noreply.github.com"))
                });
                let out = git::commit(&sandbox, &dir, &message, author.as_ref()).await?;
                self.load_projects().await;
                Ok(Some(first_line(&out).unwrap_or_else(|| "Committed.".into())))
            }

            Command::Push => {
                let (sandbox, dir) = self.project()?;
                git::push(&sandbox, &dir, mc_github::token::get()).await?;
                self.load_projects().await;
                Ok(Some("Pushed.".into()))
            }

            Command::Pull => {
                let (sandbox, dir) = self.project()?;
                let out = git::pull(&sandbox, &dir, mc_github::token::get()).await?;
                self.load_projects().await;
                Ok(Some(first_line(&out).unwrap_or_else(|| "Up to date.".into())))
            }
        }
    }

    fn sandbox(&self) -> Result<mc_sandbox::Sandbox, GithubError> {
        self.factory.create().map_err(GithubError::Other)
    }

    fn api(&self) -> Result<Api, GithubError> {
        let token = mc_github::token::get()
            .ok_or_else(|| GithubError::Other("add a GitHub token first".into()))?;
        Ok(Api::new(token.as_str()))
    }

    /// The selected project, or a clear reason there is none.
    fn project(&self) -> Result<(mc_sandbox::Sandbox, String), GithubError> {
        let dir = self
            .state
            .selected
            .clone()
            .ok_or_else(|| GithubError::Other("choose a project first".into()))?;
        Ok((self.sandbox()?, dir))
    }

    async fn load_account(&mut self) -> Result<(), GithubError> {
        let api = self.api()?;
        match api.viewer().await {
            Ok(viewer) => {
                self.state.signed_in = Some(viewer.login);
            }
            Err(GithubError::Unauthorized(reason)) => {
                // A rejected token is worse than none: every later action would
                // fail the same way, so drop it and say so.
                mc_github::token::clear();
                FORGET.store(true, Ordering::Relaxed);
                self.state.signed_in = None;
                self.state.repos.clear();
                return Err(GithubError::Unauthorized(reason));
            }
            Err(e) => return Err(e),
        }
        self.state.repos = api.repos().await.map(rows).unwrap_or_default();
        self.mark_cloned();
        Ok(())
    }

    /// Read the projects on the device and the state of the selected one.
    async fn load_projects(&mut self) {
        let Ok(sandbox) = self.sandbox() else {
            self.state.projects.clear();
            return;
        };
        let listing = sandbox
            .run(
                // One directory per line, only those that are git repositories.
                &format!(
                    "for d in {}/*/; do [ -d \"$d.git\" ] && printf '%s\\n' \"${{d%/}}\"; done",
                    self.factory.projects_dir()
                ),
                None,
            )
            .await;
        let dirs: Vec<String> = listing
            .map(|out| out.stdout.lines().map(str::to_string).filter(|d| !d.is_empty()).collect())
            .unwrap_or_default();

        // Nothing selected yet, or the selection is gone: fall back to the first.
        if self.state.selected.as_ref().is_none_or(|dir| !dirs.contains(dir)) {
            self.state.selected = dirs.first().cloned();
        }

        let mut projects = Vec::with_capacity(dirs.len());
        for dir in dirs {
            let name = dir.rsplit('/').next().unwrap_or(&dir).to_string();
            // Only the selected project's status: `git status` walks the whole
            // working tree, and doing that for every project on every refresh
            // is a phone's battery for nothing.
            let (status, clean) = if self.state.selected.as_deref() == Some(dir.as_str()) {
                match git::status(&sandbox, &dir).await {
                    Ok(status) => (Some(status.summary()), status.is_clean()),
                    Err(e) => (Some(say(e)), true),
                }
            } else {
                (None, true)
            };
            projects.push(Project { dir, name, status, clean });
        }
        self.state.projects = projects;
        self.mark_cloned();
    }

    /// Flag the repositories that are already on the device.
    fn mark_cloned(&mut self) {
        let names: Vec<&str> = self.state.projects.iter().map(|p| p.name.as_str()).collect();
        for repo in &mut self.state.repos {
            let short = repo.full_name.rsplit('/').next().unwrap_or(&repo.full_name);
            repo.cloned = names.contains(&short);
        }
    }
}

fn rows(repos: Vec<Repo>) -> Vec<RepoRow> {
    repos
        .into_iter()
        .map(|repo| RepoRow { full_name: repo.full_name, private: repo.private, cloned: false })
        .collect()
}

fn describe(command: &Command) -> String {
    match command {
        Command::SignIn(_) => "Signing in…".into(),
        Command::SignOut => "Signing out…".into(),
        Command::Refresh => "Refreshing…".into(),
        Command::Clone(repo) => format!("Cloning {repo}…"),
        Command::Create(name) => format!("Creating {name}…"),
        Command::Select(_) => "Reading the project…".into(),
        Command::Commit(_) => "Committing…".into(),
        Command::Push => "Pushing…".into(),
        Command::Pull => "Pulling…".into(),
    }
}

/// An error, as a sentence to put on screen.
fn say(error: GithubError) -> String {
    match error {
        GithubError::Unauthorized(reason) => {
            format!("GitHub rejected the token: {reason}. Add a new one.")
        }
        GithubError::Http(e) if e.is_connect() || e.is_timeout() => {
            "Could not reach GitHub. Check the network.".into()
        }
        other => other.to_string(),
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines().map(str::trim).find(|line| !line.is_empty()).map(str::to_string)
}

/// The Git pane.
///
/// A [`Component`] for the same reason as the others: its hooks need a scope of
/// their own, so switching tabs does not reorder them.
pub struct GitView {
    /// The platform collects text in a native dialog (Android) rather than
    /// inline. See [`crate::prompt`].
    pub native_prompts: bool,
    /// Owned by the app root: what the user typed, and what they are typing it
    /// for, both survive a look at another tab.
    pub draft: State<String>,
    pub asking: State<Option<Prompt>>,
}

impl PartialEq for GitView {
    fn eq(&self, other: &Self) -> bool {
        self.native_prompts == other.native_prompts
            && self.draft == other.draft
            && self.asking == other.asking
    }
}

impl Component for GitView {
    fn render(&self) -> impl IntoElement {
        body(self.native_prompts, self.draft, self.asking)
    }
}

fn body(native_prompts: bool, mut draft: State<String>, mut asking: State<Option<Prompt>>) -> impl IntoElement {
    let mut seen = use_state(|| (0u64, GitState::default()));
    use_hook(move || {
        spawn(async move {
            loop {
                async_io::Timer::after(POLL).await;
                let (version, next) = state();
                if seen.peek().0 != version {
                    seen.set((version, next));
                }
                // An answer typed into a native dialog arrives the same way.
                //
                // Read into a local first: an `if let` keeps the guard from its
                // scrutinee alive for the whole construct, so peeking inline
                // would still hold it when `set` writes - and writing to a
                // borrowed State panics on the render thread, taking the window
                // down with it (seen on the emulator, twice before this).
                let pending = *asking.peek();
                if let Some(prompt) = pending
                    && let Some(text) = prompt::take_answer(prompt)
                {
                    asking.set(None);
                    submit(prompt, text);
                }
            }
        });
    });
    let current = seen.read().1.clone();

    // Start the question, wherever it will be answered.
    let ask = move |prompt: Prompt| {
        asking.set(Some(prompt));
        draft.set(String::new());
        if native_prompts {
            prompt::request(prompt);
        }
    };

    let mut view = rect()
        .width(Size::fill())
        .height(Size::fill())
        .background(theme::GROUND)
        .color(theme::INK)
        .content(Content::Flex);

    let mut list = rect().width(Size::fill()).padding(12.).spacing(14.);
    list = list.child(account(&current, ask));

    // The inline field, on a platform that can type into one.
    if !native_prompts && let Some(prompt) = *asking.read() {
        list = list.child(inline_prompt(prompt, draft, asking));
    }

    if current.signed_in.is_some() || !current.projects.is_empty() {
        list = list.child(projects(&current, ask));
    }
    if current.signed_in.is_some() {
        list = list.child(repositories(&current));
    }

    view = view.child(
        ScrollView::new().width(Size::fill()).height(Size::flex(1.)).child(list),
    );
    view.child(footer(&current))
}

/// Who is signed in, or how to sign in.
fn account(state: &GitState, mut ask: impl FnMut(Prompt) + Clone + 'static) -> Element {
    let mut card = section("Account");
    match &state.signed_in {
        Some(login) => {
            let mut ask_repo = ask.clone();
            card = card
                .child(label().text(format!("Signed in as {login}")).font_size(15.))
                .child(
                    rect()
                        .horizontal()
                        .spacing(8.)
                        .child(
                            Button::new()
                                .compact()
                                .on_press(move |_| ask_repo(Prompt::RepoName))
                                .child("New repository"),
                        )
                        .child(
                            Button::new()
                                .compact()
                                .flat()
                                .on_press(|_| send(Command::Refresh))
                                .child("Refresh"),
                        )
                        .child(
                            Button::new()
                                .compact()
                                .flat()
                                .on_press(|_| send(Command::SignOut))
                                .child("Sign out"),
                        ),
                );
        }
        None => {
            card = card
                .child(
                    label()
                        .text(
                            "Not signed in. Make a fine-grained personal access token on \
                             github.com with Contents: read and write for the repositories you \
                             want, and paste it here. It is stored encrypted on this device and \
                             never goes into a repository, a command line or the log.",
                        )
                        .font_size(13.)
                        .color(theme::MUTED),
                )
                .child(
                    Button::new()
                        .on_press(move |_| ask(Prompt::GithubToken))
                        .child("Add token"),
                );
        }
    }
    card.into()
}

/// The projects on the device, and what can be done to the selected one.
fn projects(state: &GitState, ask: impl FnMut(Prompt) + Clone + 'static) -> Element {
    let mut card = section("Projects");
    if state.projects.is_empty() {
        card = card.child(
            label()
                .text("Nothing cloned yet. Clone a repository below, or make a new one.")
                .font_size(13.)
                .color(theme::MUTED),
        );
        return card.into();
    }

    for project in &state.projects {
        let selected = state.selected.as_deref() == Some(project.dir.as_str());
        let dir = project.dir.clone();
        // Pressable: the name and its status line. *Not* the whole card - a
        // press on the card reaches the buttons inside it as well, so tapping
        // Push also re-selected the project, and the refresh that followed
        // replaced the push's result with nothing. That is what "no success and
        // no error" looked like from the outside.
        let mut heading = rect()
            .width(Size::fill())
            .on_press(move |_| send(Command::Select(dir.clone())))
            .child(
                label()
                    .text(project.name.clone())
                    .font_size(15.)
                    .color(if selected { theme::INK } else { theme::MUTED }),
            );
        if selected && let Some(status) = &project.status {
            heading = heading
                .child(label().text(status.clone()).font_size(12.).color(theme::MUTED));
        }

        let mut row = rect()
            .width(Size::fill())
            .padding(8.)
            .spacing(4.)
            .corner_radius(8.)
            .background(if selected { theme::SURFACE } else { theme::GROUND })
            .child(heading);

        if selected {
            let mut ask_message = ask.clone();
            let can_commit = !project.clean;
            row = row.child(
                rect()
                    .horizontal()
                    .spacing(8.)
                    .child(
                        Button::new()
                            .compact()
                            .enabled(can_commit)
                            .on_press(move |_| ask_message(Prompt::CommitMessage))
                            .child("Commit"),
                    )
                    .child(
                        Button::new()
                            .compact()
                            .flat()
                            .on_press(|_| send(Command::Push))
                            .child("Push"),
                    )
                    .child(
                        Button::new()
                            .compact()
                            .flat()
                            .on_press(|_| send(Command::Pull))
                            .child("Pull"),
                    ),
            );
        }
        card = card.child(row);
    }
    card.into()
}

/// The repositories on GitHub, with the ones already here marked.
fn repositories(state: &GitState) -> Element {
    let mut card = section("Repositories");
    if state.repos.is_empty() {
        card = card.child(
            label()
                .text("No repositories, or the token cannot see any.")
                .font_size(13.)
                .color(theme::MUTED),
        );
    }
    for repo in &state.repos {
        let full_name = repo.full_name.clone();
        card = card.child(
            rect()
                .content(Content::Flex)
                .horizontal()
                .width(Size::fill())
                .cross_align(Alignment::Center)
                .spacing(8.)
                .child(
                    rect()
                        .width(Size::flex(1.))
                        .child(label().text(repo.full_name.clone()).font_size(14.).max_lines(1))
                        .child(
                            label()
                                .text(if repo.private { "private" } else { "public" })
                                .font_size(11.)
                                .color(theme::MUTED),
                        ),
                )
                .child(if repo.cloned {
                    // Already here: cloning again would fail on a non-empty
                    // directory, and saying so up front beats an error.
                    label().text("on device").font_size(12.).color(theme::MUTED).into_element()
                } else {
                    Button::new()
                        .compact()
                        .on_press(move |_| send(Command::Clone(full_name.clone())))
                        .child("Clone")
                        .into_element()
                }),
        );
    }
    card.into()
}

/// The inline field used where Freya can receive typed text - desktop.
fn inline_prompt(prompt: Prompt, mut draft: State<String>, mut asking: State<Option<Prompt>>) -> Element {
    let mut submit_now = move |text: String| {
        asking.set(None);
        spawn(async move { draft.set(String::new()) });
        submit(prompt, text);
    };
    let mut on_press = submit_now;
    rect()
        .width(Size::fill())
        .padding(10.)
        .spacing(8.)
        .corner_radius(8.)
        .background(theme::SURFACE)
        .child(label().text(prompt.title()).font_size(13.).color(theme::MUTED))
        .child(
            rect()
                .content(Content::Flex)
                .horizontal()
                .width(Size::fill())
                .spacing(8.)
                .cross_align(Alignment::Center)
                .child(
                    Input::new(draft)
                        .placeholder(prompt.hint())
                        .width(Size::flex(1.))
                        .on_submit(move |text: String| submit_now(text)),
                )
                .child(
                    Button::new()
                        .compact()
                        .on_press(move |_| {
                            // Copy out before calling: the handler writes to the
                            // same State, and writing to a borrowed one panics.
                            let text = draft.read().clone();
                            on_press(text);
                        })
                        .child("OK"),
                )
                .child(
                    Button::new()
                        .compact()
                        .flat()
                        .on_press(move |_| asking.set(None))
                        .child("Cancel"),
                ),
        )
        .into()
}

/// What just happened, or what is happening now.
fn footer(state: &GitState) -> Element {
    let text = state
        .busy
        .clone()
        .or_else(|| state.message.clone())
        .unwrap_or_else(|| "Ready".into());
    rect()
        .width(Size::fill())
        .padding((6., 12.))
        .background(theme::SURFACE)
        .child(label().text(text).font_size(12.).color(theme::MUTED).max_lines(3))
        .into()
}

fn section(title: &str) -> Rect {
    rect()
        .width(Size::fill())
        .spacing(8.)
        .child(
            label()
                .text(title.to_string())
                .font_size(12.)
                .color(theme::ACCENT)
                .font_weight(FontWeight::SEMI_BOLD),
        )
}

/// Turn an answered prompt into the command it was asked for.
fn submit(prompt: Prompt, text: String) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    send(match prompt {
        Prompt::GithubToken => Command::SignIn(text),
        Prompt::RepoName => Command::Create(text),
        Prompt::CommitMessage => Command::Commit(text),
    });
}
