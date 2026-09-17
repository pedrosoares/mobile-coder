//! [`FileBrowser`] over a rootfs directory on the host.
//!
//! The hard part is symlinks. A link inside the rootfs holds a *guest* path:
//! Alpine's `/bin/sh -> /bin/busybox`. Following it with the host filesystem
//! would read the *host's* `/bin/busybox` - on desktop, a real file outside the
//! sandbox. So links are resolved by hand, inside the root.
//!
//! One exception, found on device: proot's `--link2symlink` (needed because
//! Android forbids hard links) replaces hard links with symlinks holding
//! absolute *host* paths back into the rootfs. A target already under the root
//! is taken as-is; anything else absolute is re-rooted.

use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use mc_core::{
    FileBrowser, FileEntry, FilePreview,
    files::{EntryKind, normalize},
};

/// Links followed before giving up - a cycle, or a very long chain.
const MAX_LINK_HOPS: usize = 40;

#[derive(Debug, Clone)]
pub struct RootedFs {
    root: PathBuf,
    home: String,
}

impl RootedFs {
    pub fn new(root: impl Into<PathBuf>, home: impl Into<String>) -> Self {
        let root = root.into();
        // Canonical, so "is this under the root" is a prefix check that cannot
        // be fooled by the root itself being reached through a symlink.
        let root = fs::canonicalize(&root).unwrap_or(root);
        Self { root, home: normalize(&home.into()) }
    }

    /// Resolve every symlink in a guest path, staying inside the root.
    fn resolve(&self, guest: &str) -> Result<PathBuf, String> {
        let mut pending: Vec<String> = normalize(guest)
            .split('/')
            .filter(|p| !p.is_empty())
            .rev()
            .map(str::to_string)
            .collect();
        let mut resolved = self.root.clone();
        let mut hops = 0;

        while let Some(part) = pending.pop() {
            if part == ".." {
                if resolved != self.root {
                    resolved.pop();
                }
                continue;
            }
            let candidate = resolved.join(&part);
            let meta = fs::symlink_metadata(&candidate)
                .map_err(|e| format!("{}: {e}", self.guest_of(&candidate)))?;

            if !meta.file_type().is_symlink() {
                resolved = candidate;
                continue;
            }

            hops += 1;
            if hops > MAX_LINK_HOPS {
                return Err(format!("{guest}: too many levels of symbolic links"));
            }
            let target = fs::read_link(&candidate).map_err(|e| e.to_string())?;

            let (base, rest) = if target.starts_with(&self.root) {
                // A --link2symlink host path back into the rootfs.
                (self.root.clone(), target.strip_prefix(&self.root).unwrap_or(&target).to_path_buf())
            } else if target.is_absolute() {
                // A guest path: re-root it.
                (self.root.clone(), target.clone())
            } else {
                (resolved.clone(), target.clone())
            };

            resolved = base;
            let mut expanded: Vec<String> = rest
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    Component::ParentDir => Some("..".into()),
                    _ => None,
                })
                .collect();
            expanded.reverse();
            pending.extend(expanded);
        }

        // Belt and braces: whatever happened above, never hand back a path
        // outside the root.
        if !resolved.starts_with(&self.root) {
            return Err(format!("{guest}: resolves outside the sandbox"));
        }
        Ok(resolved)
    }

    fn guest_of(&self, host: &Path) -> String {
        let rel = host.strip_prefix(&self.root).unwrap_or(host);
        normalize(&rel.to_string_lossy())
    }
}

impl FileBrowser for RootedFs {
    fn home(&self) -> String {
        self.home.clone()
    }

    fn list(&self, path: &str) -> Result<Vec<FileEntry>, String> {
        let dir = self.resolve(path)?;
        let reader = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", normalize(path)))?;

        let mut entries: Vec<FileEntry> = reader
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let meta = entry.metadata().ok();
                let file_type = entry.file_type().ok();
                let kind = match file_type {
                    Some(t) if t.is_symlink() => {
                        // Show a link to a directory as a directory, so it can
                        // be opened; resolved inside the root, never on the host.
                        let target = self.resolve(&mc_core::files::join(path, &name));
                        match target.ok().and_then(|p| fs::metadata(p).ok()) {
                            Some(m) if m.is_dir() => EntryKind::Directory,
                            _ => EntryKind::Symlink,
                        }
                    }
                    Some(t) if t.is_dir() => EntryKind::Directory,
                    Some(t) if t.is_file() => EntryKind::File,
                    _ => EntryKind::Other,
                };
                let size = meta.filter(|m| m.is_file()).map(|m| m.len());
                FileEntry { name, kind, size }
            })
            // proot's --link2symlink bookkeeping: an implementation detail of
            // hard-link emulation, not something a user created.
            .filter(|e| !e.name.starts_with(".l2s."))
            .collect();

        entries.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    fn preview(&self, path: &str, limit: usize) -> Result<FilePreview, String> {
        let host = self.resolve(path)?;
        let meta = fs::metadata(&host).map_err(|e| format!("{}: {e}", normalize(path)))?;
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", normalize(path)));
        }

        let mut buffer = Vec::with_capacity(limit.min(meta.len() as usize));
        fs::File::open(&host)
            .and_then(|f| f.take(limit as u64).read_to_end(&mut buffer))
            .map_err(|e| format!("{}: {e}", normalize(path)))?;

        // A NUL byte in the first stretch is the usual cheap binary test.
        if buffer.iter().take(8000).any(|&b| b == 0) {
            return Ok(FilePreview::Binary { size: meta.len() });
        }
        Ok(FilePreview::Text {
            text: String::from_utf8_lossy(&buffer).into_owned(),
            truncated: meta.len() > limit as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// A tiny rootfs next to a decoy "host" file it must never reach.
    fn fixture(name: &str) -> (PathBuf, RootedFs) {
        let base = std::env::temp_dir().join(format!("mc-rootedfs-{name}"));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("rootfs");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("root/src")).unwrap();
        fs::write(root.join("bin/busybox"), b"GUEST BUSYBOX").unwrap();
        fs::write(root.join("root/hello.c"), b"int main(void){return 0;}").unwrap();
        fs::write(base.join("host-secret"), b"HOST FILE").unwrap();
        let fs_ = RootedFs::new(&root, "/root");
        (base, fs_)
    }

    #[test]
    fn an_absolute_guest_symlink_resolves_inside_the_rootfs_not_on_the_host() {
        let (base, fs_) = fixture("absolute");
        // Exactly Alpine's layout.
        symlink("/bin/busybox", base.join("rootfs/bin/sh")).unwrap();
        match fs_.preview("/bin/sh", 1024).unwrap() {
            FilePreview::Text { text, .. } => assert_eq!(text, "GUEST BUSYBOX"),
            other => panic!("{other:?}"),
        }
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn dotdot_and_link_tricks_cannot_escape_the_root() {
        let (base, fs_) = fixture("escape");
        let secret = base.join("host-secret");
        // A relative link that climbs out, and an absolute one straight at it.
        symlink("../../host-secret", base.join("rootfs/root/climb")).unwrap();
        symlink(&secret, base.join("rootfs/root/direct")).unwrap();

        for path in ["/../host-secret", "/root/climb", "/root/direct"] {
            let leaked = matches!(
                fs_.preview(path, 1024),
                Ok(FilePreview::Text { ref text, .. }) if text.contains("HOST FILE")
            );
            assert!(!leaked, "{path} reached a file outside the rootfs");
        }
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_link2symlink_host_path_back_into_the_rootfs_is_followed() {
        let (base, fs_) = fixture("l2s");
        let root = fs::canonicalize(base.join("rootfs")).unwrap();
        // What --link2symlink leaves behind for a hard link.
        fs::write(root.join("bin/.l2s.gcc0001"), b"REAL GCC").unwrap();
        symlink(root.join("bin/.l2s.gcc0001"), root.join("bin/gcc")).unwrap();

        match fs_.preview("/bin/gcc", 1024).unwrap() {
            FilePreview::Text { text, .. } => assert_eq!(text, "REAL GCC"),
            other => panic!("{other:?}"),
        }
        let names: Vec<String> = fs_.list("/bin").unwrap().into_iter().map(|e| e.name).collect();
        assert!(!names.iter().any(|n| n.starts_with(".l2s.")), "bookkeeping files are hidden: {names:?}");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_symlink_cycle_is_an_error_not_a_hang() {
        let (base, fs_) = fixture("cycle");
        symlink("/root/b", base.join("rootfs/root/a")).unwrap();
        symlink("/root/a", base.join("rootfs/root/b")).unwrap();
        assert!(fs_.preview("/root/a", 16).is_err());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn listing_puts_directories_first() {
        let (base, fs_) = fixture("list");
        let names: Vec<(String, EntryKind)> =
            fs_.list("/root").unwrap().into_iter().map(|e| (e.name, e.kind)).collect();
        assert_eq!(names, vec![("src".into(), EntryKind::Directory), ("hello.c".into(), EntryKind::File)]);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn binary_files_are_not_shown_as_text() {
        let (base, fs_) = fixture("binary");
        fs::write(base.join("rootfs/root/a.out"), [0x7f, b'E', b'L', b'F', 0, 0, 1]).unwrap();
        assert!(matches!(fs_.preview("/root/a.out", 1024), Ok(FilePreview::Binary { size: 7 })));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn large_files_are_truncated_and_say_so() {
        let (base, fs_) = fixture("large");
        fs::write(base.join("rootfs/root/big.txt"), "x".repeat(5000)).unwrap();
        match fs_.preview("/root/big.txt", 1000).unwrap() {
            FilePreview::Text { text, truncated } => {
                assert_eq!(text.len(), 1000);
                assert!(truncated);
            }
            other => panic!("{other:?}"),
        }
        let _ = fs::remove_dir_all(base);
    }
}
