//! How a binary actually gets executed on Android.
//!
//! This module exists because of one Android rule, and it is the rule the whole
//! project rests on:
//!
//! An app whose `targetSdkVersion` is >= 29 runs in an SELinux domain that
//! forbids `exec()` on any file under `/data/data/<pkg>`. Google calls executing
//! from the writable app home directory a W^X violation. proot does not help:
//! proot translates *paths*, but the kernel still performs the `execve`, and the
//! kernel still refuses.
//!
//! The documented escape - the one `termux-exec` uses - is to invoke the system
//! linker explicitly:
//!
//! ```text
//! execve("/system/bin/linker64", ["/data/data/<pkg>/files/rootfs/bin/sh", ...])
//! ```
//!
//! The kernel only ever sees `/system/bin/linker64` being executed, which is
//! permitted, and the linker then loads the real program itself.
//!
//! # The open question
//!
//! `/system/bin/linker64` is *bionic's* loader. Whether it can load a binary from
//! a foreign-libc rootfs - musl on Alpine, glibc on Debian - is not something we
//! are willing to assume in either direction. `tools/exec-probe` answers it on
//! real hardware. Until it reports back, treat [`ExecStrategy::SystemLinker`] as
//! unproven for guest binaries and proven only for bionic ones.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The bionic linker for 64-bit processes.
pub const SYSTEM_LINKER_64: &str = "/system/bin/linker64";
/// The bionic linker for 32-bit processes.
pub const SYSTEM_LINKER_32: &str = "/system/bin/linker";

/// Environment variable carrying the program's real path.
///
/// Under [`ExecStrategy::SystemLinker`], `/proc/self/exe` reports the linker
/// rather than the program, which breaks anything that re-executes itself or
/// locates its own assets that way. `termux-exec` solves this with an env var and
/// so do we; ours is namespaced to avoid colliding with theirs if both are present.
pub const PROC_SELF_EXE_VAR: &str = "MC_EXEC__PROC_SELF_EXE";

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(
        "the system linker needs an absolute program path, got {0:?}; \
         resolve it against the rootfs before spawning"
    )]
    RelativePath(PathBuf),
}

/// How to turn "run this program" into an actual `execve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecStrategy {
    /// Plain `execve` of the program itself.
    ///
    /// Correct on desktop, and on Android only when `targetSdkVersion <= 28`.
    Direct,

    /// Route through the Android system linker. Required for `targetSdkVersion >= 29`.
    SystemLinker { linker: PathBuf },
}

/// A command rewritten into the form that should actually be spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Extra environment the strategy requires. Merge, do not replace.
    pub env: Vec<(String, OsString)>,
}

impl ExecStrategy {
    /// Pick a strategy for the platform we were compiled for.
    ///
    /// We choose `SystemLinker` on Android unconditionally rather than sniffing
    /// `targetSdkVersion` at runtime. Routing a bionic binary through the linker
    /// works whether or not the restriction applies, so the worst case is a
    /// redundant hop; guessing the other way is a hard failure.
    pub fn detect() -> Self {
        #[cfg(target_os = "android")]
        {
            let linker = if cfg!(target_pointer_width = "64") {
                SYSTEM_LINKER_64
            } else {
                SYSTEM_LINKER_32
            };
            Self::SystemLinker {
                linker: PathBuf::from(linker),
            }
        }
        #[cfg(not(target_os = "android"))]
        {
            Self::Direct
        }
    }

    /// Rewrite a program plus arguments into what should be spawned.
    pub fn resolve<I, S>(&self, program: &Path, args: I) -> Result<ResolvedCommand, ExecError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let args = args.into_iter().map(Into::into);
        match self {
            Self::Direct => Ok(ResolvedCommand {
                program: program.to_path_buf(),
                args: args.collect(),
                env: Vec::new(),
            }),

            Self::SystemLinker { linker } => {
                // The linker resolves nothing itself - a relative path here fails
                // at runtime with a message that points nowhere near the cause.
                if !program.is_absolute() {
                    return Err(ExecError::RelativePath(program.to_path_buf()));
                }

                // argv becomes [<program>, <original args...>]. The program path
                // is the linker's first operand *and* stays argv[0], so the child
                // sees the argv it expects.
                let mut linker_args = Vec::new();
                linker_args.push(OsString::from(program));
                linker_args.extend(args);

                Ok(ResolvedCommand {
                    program: linker.clone(),
                    args: linker_args,
                    env: vec![(
                        PROC_SELF_EXE_VAR.to_string(),
                        OsString::from(program),
                    )],
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linker() -> ExecStrategy {
        ExecStrategy::SystemLinker {
            linker: PathBuf::from(SYSTEM_LINKER_64),
        }
    }

    #[test]
    fn direct_passes_the_command_through_untouched() {
        let cmd = ExecStrategy::Direct
            .resolve(Path::new("/bin/sh"), ["-lc", "echo hi"])
            .unwrap();
        assert_eq!(cmd.program, PathBuf::from("/bin/sh"));
        assert_eq!(cmd.args, vec![OsString::from("-lc"), OsString::from("echo hi")]);
        assert!(cmd.env.is_empty());
    }

    #[test]
    fn system_linker_becomes_the_program_and_the_target_leads_argv() {
        let cmd = linker()
            .resolve(Path::new("/data/data/x/files/rootfs/bin/sh"), ["-lc", "echo hi"])
            .unwrap();
        assert_eq!(cmd.program, PathBuf::from(SYSTEM_LINKER_64));
        assert_eq!(
            cmd.args,
            vec![
                OsString::from("/data/data/x/files/rootfs/bin/sh"),
                OsString::from("-lc"),
                OsString::from("echo hi"),
            ]
        );
    }

    #[test]
    fn system_linker_exports_the_real_exe_path() {
        let cmd = linker().resolve(Path::new("/abs/prog"), Vec::<String>::new()).unwrap();
        assert_eq!(
            cmd.env,
            vec![(PROC_SELF_EXE_VAR.to_string(), OsString::from("/abs/prog"))]
        );
    }

    #[test]
    fn system_linker_rejects_a_relative_path_rather_than_failing_at_runtime() {
        let err = linker().resolve(Path::new("bin/sh"), Vec::<String>::new());
        assert!(matches!(err, Err(ExecError::RelativePath(_))));
    }
}
