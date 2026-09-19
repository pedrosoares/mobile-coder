//! How many background commands are running, for the UI to show.
//!
//! The jobs themselves live in `mc-sandbox`, which owns processes; `mc-core`
//! depends on no other crate in the workspace, and the UI depends on `mc-core`
//! but deliberately not on the sandbox. A single number crosses that gap, set
//! by whoever runs the jobs and read by whoever draws the screen.
//!
//! Why show it at all: a process running with nothing on screen to say so is
//! how a phone ends up warm in a pocket.

use std::sync::atomic::{AtomicUsize, Ordering};

static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// How many background jobs are running.
pub fn running() -> usize {
    RUNNING.load(Ordering::Relaxed)
}

/// Publish the current count. Called by the job registry, not by the UI.
pub fn set_running(count: usize) {
    RUNNING.store(count, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_round_trips() {
        set_running(3);
        assert_eq!(running(), 3);
        set_running(0);
        assert_eq!(running(), 0);
    }
}
