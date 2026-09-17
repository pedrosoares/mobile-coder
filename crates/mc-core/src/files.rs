//! Browsing the project's files, in guest terms.
//!
//! Paths here are what the agent sees inside the sandbox - `/root/hello.c` - never
//! where that happens to live on the device. The UI browses through
//! [`FileBrowser`] and stays ignorant of the rootfs location, the same boundary
//! [`crate::Project::path`] keeps.

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    // Declared in display order: directories first.
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Bytes, for files.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePreview {
    Text { text: String, truncated: bool },
    /// Not shown as text: rendering arbitrary bytes as UTF-8 is noise.
    Binary { size: u64 },
}

/// Read access to the sandbox's files. Implemented by `mc-sandbox`.
pub trait FileBrowser: Send + Sync {
    /// Where browsing starts - the project directory.
    fn home(&self) -> String;
    /// Entries of a directory, sorted directories first, then by name.
    fn list(&self, path: &str) -> Result<Vec<FileEntry>, String>;
    /// The start of a file, up to `limit` bytes.
    fn preview(&self, path: &str, limit: usize) -> Result<FilePreview, String>;
}

/// Normalise a guest path: absolute, no `.`/`..`/empty segments.
///
/// `..` pops rather than being refused, so "/root/.." is "/". It cannot climb
/// above "/", which is what keeps a guest path inside the rootfs once joined.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

pub fn join(dir: &str, name: &str) -> String {
    normalize(&format!("{dir}/{name}"))
}

/// The parent directory, or `None` at the root.
pub fn parent(path: &str) -> Option<String> {
    let path = normalize(path);
    if path == "/" {
        return None;
    }
    Some(normalize(&format!("{path}/..")))
}

/// Human-readable size.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_cannot_climb_above_the_root() {
        assert_eq!(normalize("/root/../../../etc"), "/etc");
        assert_eq!(normalize("../../x"), "/x");
        assert_eq!(normalize("//root/./a//b/"), "/root/a/b");
        assert_eq!(normalize(""), "/");
    }

    #[test]
    fn parent_and_join_walk_the_tree() {
        assert_eq!(parent("/root/src"), Some("/root".into()));
        assert_eq!(parent("/root"), Some("/".into()));
        assert_eq!(parent("/"), None);
        assert_eq!(join("/root", "hello.c"), "/root/hello.c");
        assert_eq!(join("/root", ".."), "/");
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
