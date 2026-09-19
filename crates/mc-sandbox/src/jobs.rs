//! Commands that outlive the turn that started them.
//!
//! A tool call blocks its turn, which is right for a build and wrong for a
//! server: `npm run dev`, `cargo watch`, `tail -f` are *supposed* to keep
//! running while the conversation continues. Those go here instead - started,
//! left alone, read from later, killed when they are no longer wanted.
//!
//! The registry is process-wide rather than per-[`crate::Sandbox`], because a
//! `Sandbox` is rebuilt for every turn (see `worker::Setup`) while a job is
//! meant to survive exactly that.
//!
//! What a job is *not*: a way around the deadline on a foreground command. A
//! background job has no deadline at all, so everything here is built around
//! being able to see it and end it - a list, a bounded buffer, a kill, and a
//! count the UI can show.
//!
//! # When the app dies
//!
//! A job does not outlive the app, and nothing here has to make sure of that.
//! Android tears down an app's process group when the app process goes, and the
//! guest goes with it: measured on the Fold6 with a job writing a line a
//! second, it was gone after `am force-stop`, and gone after a bare `kill -9`
//! of the app process - the shape a low-memory kill takes - proot and the shell
//! under it included.
//!
//! The obvious belt-and-braces, sweeping `/proc` at startup for processes
//! running out of this rootfs, is not available to an app: Android mounts
//! `/proc` with `hidepid`, and from inside the app the scan returned exactly
//! one entry, itself. A `run-as` shell sees a thousand, which is what makes
//! this easy to get wrong from a terminal - `run-as` keeps the shell's
//! `AID_READPROC` group, and the app has no such group.
//!
//! So what has to work is the process group, which is what makes a kill from
//! *inside* the running app reach everything a job started. See
//! [`crate::Sandbox::start_background`].

use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::io::AsyncReadExt;

/// How much of a job's output is kept in memory, per job.
///
/// The tail, not the head: a server that has been up for an hour is read for
/// what it just logged. This is a phone, so the bound matters more than the
/// history - anything that must be kept should be redirected to a file.
const MAX_BUFFER: usize = 256 * 1024;

/// How many finished jobs to keep around for their output. Running jobs are
/// never pruned.
const MAX_FINISHED: usize = 16;

/// What a job is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    /// Ended on its own. `None` when it was ended by a signal.
    Exited(Option<i32>),
    /// Ended because someone asked for it to be.
    Killed,
}

impl JobStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, JobStatus::Running)
    }

    /// One line, for a tool result or a list.
    pub fn describe(&self) -> String {
        match self {
            JobStatus::Running => "running".into(),
            JobStatus::Exited(Some(0)) => "exited 0".into(),
            JobStatus::Exited(Some(code)) => format!("exited {code}"),
            JobStatus::Exited(None) => "ended by a signal".into(),
            JobStatus::Killed => "killed".into(),
        }
    }
}

/// A job, without its output.
#[derive(Debug, Clone)]
pub struct JobSummary {
    pub id: String,
    pub command: String,
    pub status: JobStatus,
    pub age: Duration,
    /// Output produced but not yet read.
    pub unread: usize,
}

impl JobSummary {
    /// The form the model reads: `job-1  running  12s  npm run dev`.
    pub fn describe(&self) -> String {
        format!(
            "{}  {}  {}s  {}",
            self.id,
            self.status.describe(),
            self.age.as_secs(),
            first_line(&self.command),
        )
    }
}

/// A read of a job: everything since the last read, plus where it stands.
#[derive(Debug, Clone)]
pub struct JobRead {
    pub summary: JobSummary,
    pub output: String,
    /// Bytes dropped from the front because the buffer was full.
    pub dropped: usize,
}

/// A job's output, kept to [`MAX_BUFFER`] with a read cursor.
#[derive(Debug, Default)]
struct Buffer {
    text: String,
    /// How far into `text` has been read already.
    cursor: usize,
    /// How much was dropped from the front, in total.
    dropped: usize,
}

impl Buffer {
    fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        if self.text.len() <= MAX_BUFFER {
            return;
        }
        // Drop from the front, on a character boundary so the string stays
        // valid - the buffer holds decoded text, not bytes.
        let mut cut = self.text.len() - MAX_BUFFER;
        while cut < self.text.len() && !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        self.text.drain(..cut);
        self.dropped += cut;
        self.cursor = self.cursor.saturating_sub(cut);
    }

    fn take(&mut self) -> String {
        let out = self.text[self.cursor..].to_string();
        self.cursor = self.text.len();
        out
    }

    fn unread(&self) -> usize {
        self.text.len() - self.cursor
    }
}

#[derive(Debug)]
struct Job {
    id: String,
    command: String,
    started: Instant,
    /// The process group to signal. Its leader is the process we spawned.
    pgid: u32,
    buffer: Mutex<Buffer>,
    status: Mutex<JobStatus>,
    /// When it stopped running, for pruning.
    finished: Mutex<Option<Instant>>,
}

impl Job {
    fn summary(&self) -> JobSummary {
        JobSummary {
            id: self.id.clone(),
            command: self.command.clone(),
            status: self.status.lock().map(|s| s.clone()).unwrap_or(JobStatus::Running),
            age: self.started.elapsed(),
            unread: self.buffer.lock().map(|b| b.unread()).unwrap_or(0),
        }
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Job>>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

/// How many jobs are running now. The chat shows this - a process with no
/// visible trace is exactly the kind of thing a phone should not have.
pub fn running_count() -> usize {
    registry()
        .lock()
        .map(|jobs| jobs.values().filter(|j| j.summary().status.is_running()).count())
        .unwrap_or(0)
}

/// Republish the count for the UI. Called whenever a job starts or ends.
fn publish_count() {
    mc_core::jobs::set_running(running_count());
}

/// Start `proc` as a job and return its id.
///
/// `command` is what the caller asked for, kept for the listing; `proc` must be
/// already configured (stdio piped, own process group) - see
/// [`crate::Sandbox::start_background`], which is how this is meant to be
/// reached.
pub fn start(command: String, mut proc: tokio::process::Command) -> io::Result<String> {
    let mut child = proc.spawn()?;
    let pgid = child.id().ok_or_else(|| {
        io::Error::other("the job exited before it could be registered")
    })?;

    let id = format!("job-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let job = Arc::new(Job {
        id: id.clone(),
        command,
        started: Instant::now(),
        pgid,
        buffer: Mutex::new(Buffer::default()),
        status: Mutex::new(JobStatus::Running),
        finished: Mutex::new(None),
    });

    // Both streams into one buffer, in the order they arrive: a server's log
    // interleaves them, and splitting them apart would be a lie about ordering.
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump(stdout, Arc::clone(&job)));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump(stderr, Arc::clone(&job)));
    }

    if let Ok(mut jobs) = registry().lock() {
        jobs.insert(id.clone(), Arc::clone(&job));
    }
    publish_count();

    // Reap it, so its status stops being a guess.
    let waiting = Arc::clone(&job);
    tokio::spawn(async move {
        let status = child.wait().await;
        let outcome = match status {
            Ok(status) => match status.code() {
                Some(code) => JobStatus::Exited(Some(code)),
                None => JobStatus::Exited(None),
            },
            Err(_) => JobStatus::Exited(None),
        };
        finish(&waiting, outcome);
    });

    Ok(id)
}

/// Read one of a job's streams into its buffer until the stream ends.
async fn pump<R: tokio::io::AsyncRead + Unpin>(mut reader: R, job: Arc<Job>) {
    let mut chunk = [0u8; 8192];
    // A read can land in the middle of a multi-byte character; carry the tail
    // to the next one rather than turning it into replacement characters.
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        pending.extend_from_slice(&chunk[..read]);
        let text = match std::str::from_utf8(&pending) {
            Ok(text) => {
                let text = text.to_string();
                pending.clear();
                text
            }
            Err(e) => {
                let good = e.valid_up_to();
                let text = String::from_utf8_lossy(&pending[..good]).into_owned();
                pending.drain(..good);
                // Longer than any character: genuinely not UTF-8, so stop
                // holding on to it.
                if pending.len() > 4 {
                    pending.clear();
                }
                text
            }
        };
        if text.is_empty() {
            continue;
        }
        if let Ok(mut buffer) = job.buffer.lock() {
            buffer.push(&text);
        }
    }
}

/// Mark a job finished, unless it was already marked (a kill beats the reaper).
fn finish(job: &Arc<Job>, outcome: JobStatus) {
    if let Ok(mut status) = job.status.lock()
        && status.is_running()
    {
        *status = outcome;
    }
    if let Ok(mut at) = job.finished.lock() {
        at.get_or_insert_with(Instant::now);
    }
    prune();
    publish_count();
}

/// Everything a job has printed since the last read.
pub fn read(id: &str) -> Option<JobRead> {
    let job = find(id)?;
    let (output, dropped) = match job.buffer.lock() {
        Ok(mut buffer) => (buffer.take(), buffer.dropped),
        Err(_) => (String::new(), 0),
    };
    Some(JobRead { summary: job.summary(), output, dropped })
}

/// Kill a job and everything it started. Returns `None` for an unknown id.
pub fn kill(id: &str) -> Option<JobSummary> {
    let job = find(id)?;
    if job.summary().status.is_running() {
        kill_group(job.pgid);
        finish(&job, JobStatus::Killed);
    }
    Some(job.summary())
}

/// Every job this process knows about, oldest first.
pub fn list() -> Vec<JobSummary> {
    let Ok(jobs) = registry().lock() else { return Vec::new() };
    let mut all: Vec<JobSummary> = jobs.values().map(|job| job.summary()).collect();
    // By age descending is by start time ascending: the order they were made.
    all.sort_by_key(|job| std::cmp::Reverse(job.age));
    all
}

/// Kill every running job. For a shutdown, or a user who wants the phone back.
pub fn kill_all() -> usize {
    list()
        .into_iter()
        .filter(|job| job.status.is_running())
        .filter_map(|job| kill(&job.id))
        .count()
}

fn find(id: &str) -> Option<Arc<Job>> {
    registry().lock().ok()?.get(id).cloned()
}

/// Forget the oldest finished jobs, keeping [`MAX_FINISHED`].
fn prune() {
    let Ok(mut jobs) = registry().lock() else { return };
    let mut finished: Vec<(String, Instant)> = jobs
        .values()
        .filter(|job| !job.summary().status.is_running())
        .filter_map(|job| job.finished.lock().ok()?.map(|at| (job.id.clone(), at)))
        .collect();
    if finished.len() <= MAX_FINISHED {
        return;
    }
    finished.sort_by_key(|(_, at)| *at);
    for (id, _) in finished.iter().take(finished.len() - MAX_FINISHED) {
        jobs.remove(id);
    }
}

#[cfg(unix)]
fn kill_group(pgid: u32) {
    // SAFETY: a signal to a process group this process created.
    unsafe {
        libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pgid: u32) {}

/// The first line of a command, for a one-line listing.
fn first_line(command: &str) -> String {
    command.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_keeps_the_tail_and_says_what_it_dropped() {
        let mut buffer = Buffer::default();
        buffer.push(&"a".repeat(MAX_BUFFER));
        buffer.push("tail");
        assert_eq!(buffer.text.len(), MAX_BUFFER);
        assert!(buffer.text.ends_with("tail"));
        assert_eq!(buffer.dropped, 4);
    }

    #[test]
    fn a_read_returns_only_what_is_new() {
        let mut buffer = Buffer::default();
        buffer.push("first\n");
        assert_eq!(buffer.take(), "first\n");
        assert_eq!(buffer.take(), "");
        buffer.push("second\n");
        assert_eq!(buffer.take(), "second\n");
    }

    #[test]
    fn dropping_moves_the_cursor_with_the_text() {
        let mut buffer = Buffer::default();
        buffer.push("old");
        buffer.take();
        buffer.push(&"x".repeat(MAX_BUFFER));
        // "old" fell off the front, taking the cursor with it - so the read
        // that follows returns the new text once, and not a byte of the old.
        assert_eq!(buffer.dropped, 3);
        assert_eq!(buffer.unread(), MAX_BUFFER);
        assert_eq!(buffer.take(), "x".repeat(MAX_BUFFER));
        assert_eq!(buffer.take(), "");
    }

    #[tokio::test]
    async fn a_job_runs_past_the_call_that_started_it_until_it_is_killed() {
        let dir = std::env::temp_dir().join(format!("mc-job-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("survived");

        let sandbox = crate::Sandbox::host();
        let id = sandbox
            .start_background(
                // Prints as it goes, leaves a grandchild behind, and would run
                // far longer than any test.
                &format!(
                    "(sleep 3; touch {}) & for i in 1 2 3; do echo line $i; sleep 0.2; done; sleep 60",
                    marker.display()
                ),
                None,
            )
            .expect("the job should start");

        // It is running, and the call that started it already returned.
        assert!(list().iter().any(|job| job.id == id));
        tokio::time::sleep(Duration::from_millis(900)).await;

        let first = read(&id).expect("the job should be readable");
        assert!(first.summary.status.is_running(), "{:?}", first.summary.status);
        assert!(first.output.contains("line 1"), "output was {:?}", first.output);
        assert!(first.output.contains("line 3"), "output was {:?}", first.output);

        // A read takes what it read: the model should not see it twice.
        let second = read(&id).expect("still readable");
        assert!(!second.output.contains("line 1"), "re-read {:?}", second.output);

        let killed = kill(&id).expect("the job should be known");
        assert_eq!(killed.status, JobStatus::Killed);
        assert!(!read(&id).unwrap().summary.status.is_running());

        // And the kill reached what the job started, not just the job.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!marker.exists(), "a process outlived the job");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unknown_job_is_reported_rather_than_panicking() {
        assert!(read("job-does-not-exist").is_none());
        assert!(kill("job-does-not-exist").is_none());
    }

    #[test]
    fn status_reads_as_a_person_would_say_it() {
        assert_eq!(JobStatus::Running.describe(), "running");
        assert_eq!(JobStatus::Exited(Some(0)).describe(), "exited 0");
        assert_eq!(JobStatus::Exited(Some(2)).describe(), "exited 2");
        assert_eq!(JobStatus::Exited(None).describe(), "ended by a signal");
        assert_eq!(JobStatus::Killed.describe(), "killed");
    }
}
