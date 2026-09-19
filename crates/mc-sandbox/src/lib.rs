//! Process execution, the rootfs, and the Android execution rules that govern both.
//!
//! Nothing in here knows that Claude exists. `mc-agent` drives it; it does not
//! call back.

pub mod android;
pub mod backend;
pub mod dns;
pub mod exec;
pub mod fs;
pub mod jobs;
pub mod rootfs;

pub use backend::{
    Backend, HostBackend, HostShell, Output, ProotBackend, Sandbox, SandboxError, SandboxFactory,
};
pub use jobs::{JobRead, JobStatus, JobSummary};
pub use exec::{ExecError, ExecStrategy, ResolvedCommand, SYSTEM_LINKER_32, SYSTEM_LINKER_64};
pub use rootfs::{Progress, RootfsError, RootfsSpec};
