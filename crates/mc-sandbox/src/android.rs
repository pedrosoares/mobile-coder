//! Android runtime facts the sandbox needs.

use std::path::{Path, PathBuf};

/// Locate the directory the APK's native libraries were unpacked into.
///
/// This is where proot, its loader and its shared libraries live, and it is the
/// one place an app may execute from at any `targetSdkVersion`.
///
/// The path is not a constant - it contains per-install random segments, e.g.
/// `/data/app/~~aB12../net.example.app-Xy9../lib/arm64`. The usual way to get it
/// is a JNI round trip to `ApplicationInfo.nativeLibraryDir`. We read
/// `/proc/self/maps` instead: our own `.so` is already mapped, so its directory
/// is right there, and it costs no JNI, no `AndroidApp` handle, and nothing that
/// has to be threaded down from the activity.
pub fn native_lib_dir(lib_name: &str) -> Option<PathBuf> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    parse_maps(&maps, lib_name)
}

/// Split out so it can be tested off-device.
fn parse_maps(maps: &str, lib_name: &str) -> Option<PathBuf> {
    maps.lines()
        .filter_map(|line| {
            // Format: addr perms offset dev inode  pathname
            // The path may contain spaces, so take everything past the 5th field.
            let path = line.split_whitespace().nth(5)?;
            path.ends_with(lib_name).then_some(Path::new(path))
        })
        .find_map(|path| path.parent().map(Path::to_path_buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
7f8a1000-7f8a2000 r--p 00000000 fd:03 1234  /system/lib64/libc.so
7f8b0000-7f8b9000 r-xp 00000000 fd:03 5678  /data/app/~~aB12cd==/net.example.app-Xy9z==/lib/arm64/libmobile_coder.so
7f8c0000-7f8c1000 rw-p 00000000 00:00 0     [anon:.bss]
";

    #[test]
    fn finds_our_own_library_directory() {
        assert_eq!(
            parse_maps(SAMPLE, "libmobile_coder.so"),
            Some(PathBuf::from(
                "/data/app/~~aB12cd==/net.example.app-Xy9z==/lib/arm64"
            ))
        );
    }

    #[test]
    fn ignores_unrelated_mappings_and_anonymous_regions() {
        assert_eq!(parse_maps(SAMPLE, "libnope.so"), None);
    }
}
