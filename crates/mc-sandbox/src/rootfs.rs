//! Getting a Linux rootfs onto the device, and unpacking it.
//!
//! Deliberately not shipped inside the APK. A rootfs is 3-8 MB compressed and
//! 30-80 MB unpacked before anyone installs a compiler, and baking that into the
//! APK costs every user the download whether or not they ever open a project.
//! Fetching on first run also means the image can be re-pinned without shipping
//! a new build.
//!
//! First run is the worst moment in this app's life: a slow, interruptible
//! download on a phone that may be on cellular data and may be backgrounded
//! mid-way. So the download resumes, the archive is checksummed before it is
//! trusted, and a marker records what was unpacked so a relaunch is free.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// Written into the rootfs once it is complete. Its presence *and* contents are
/// the readiness test: a half-extracted tree has no marker, and a tree extracted
/// from a different image has the wrong one.
const MARKER: &str = ".mc-rootfs";

#[derive(Debug, thiserror::Error)]
pub enum RootfsError {
    #[error("downloading the rootfs failed: {0}")]
    Download(#[from] reqwest::Error),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "checksum mismatch: expected {expected}, got {actual}. \
         The download was corrupted or the image moved; the archive has been discarded."
    )]
    Checksum { expected: String, actual: String },
    #[error("the server does not support resuming; delete {0} and retry")]
    ResumeUnsupported(PathBuf),
}

fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> RootfsError {
    let path = path.into();
    move |source| RootfsError::Io { path, source }
}

/// Which rootfs to install, and where.
#[derive(Debug, Clone)]
pub struct RootfsSpec {
    /// Archive URL. `.tar` and `.tar.gz` are both accepted - the format is
    /// detected from the bytes, not the name.
    pub url: String,
    /// Expected SHA-256 of the archive. Optional, but omitting it means trusting
    /// the network.
    pub sha256: Option<String>,
    /// Where the rootfs is unpacked.
    pub dest: PathBuf,
    /// Where the archive is downloaded to, so a partial file can be resumed.
    pub archive: PathBuf,
}

/// Alpine version the pinned checksums below belong to.
pub const ALPINE_VERSION: &str = "3.21.7";

impl RootfsSpec {
    /// The default rootfs: an Alpine minirootfs matching the running CPU.
    ///
    /// Pinned by checksum rather than tracking "latest", so an upstream rebuild
    /// cannot silently change what lands on a user's device. Bumping the version
    /// means bumping the hash here, deliberately.
    ///
    /// Alpine is the default because it is small, which matters most on the very
    /// first launch over cellular data. See ARCHITECTURE.md section 2.4 for why a
    /// glibc distro may suit a coding workstation better once size is less
    /// pressing - nothing here assumes Alpine beyond these two constants.
    pub fn alpine(files_dir: &Path) -> Self {
        #[cfg(target_arch = "aarch64")]
        let (arch, sha) = (
            "aarch64",
            "d1d1a3fae5f4d6146e9742790a47fcb116199622cfb8439f218a4d5fbe5000da",
        );
        #[cfg(target_arch = "x86_64")]
        let (arch, sha) = (
            "x86_64",
            "8cba1ea3e8b500ea986a313d8eecf3d5952a2a0d23a69117bb81c023d9ceac05",
        );
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        compile_error!("no pinned Alpine rootfs for this architecture");

        let branch = ALPINE_VERSION.rsplit_once('.').map(|(b, _)| b).unwrap_or("3.21");
        Self {
            url: format!(
                "https://dl-cdn.alpinelinux.org/alpine/v{branch}/releases/{arch}/\
                 alpine-minirootfs-{ALPINE_VERSION}-{arch}.tar.gz"
            )
            .replace(' ', ""),
            sha256: Some(sha.to_string()),
            dest: files_dir.join("rootfs"),
            archive: files_dir.join("rootfs-download.tar.gz"),
        }
    }
}

/// What the UI needs to render honest progress.
#[derive(Debug, Clone, Copy)]
pub enum Progress {
    /// Nothing to do - the marker matches.
    AlreadyReady,
    /// `total` is `None` when the server sends no length.
    Downloading { downloaded: u64, total: Option<u64> },
    Verifying,
    Extracting { entries: u64 },
    Ready,
}

/// Ensure the rootfs is present and unpacked, returning its path.
///
/// Safe to call on every launch: if the marker matches, it does nothing.
pub async fn ensure(
    spec: &RootfsSpec,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<PathBuf, RootfsError> {
    if is_ready(spec) {
        progress(Progress::AlreadyReady);
        return Ok(spec.dest.clone());
    }

    download_resumable(spec, progress).await?;

    if let Some(expected) = &spec.sha256 {
        progress(Progress::Verifying);
        let actual = sha256_file(&spec.archive)?;
        if !actual.eq_ignore_ascii_case(expected) {
            // Remove it: keeping a bad archive means the next run resumes onto
            // corruption and fails identically, forever.
            let _ = fs::remove_file(&spec.archive);
            return Err(RootfsError::Checksum {
                expected: expected.clone(),
                actual,
            });
        }
    }

    // Extract to a scratch directory and swap it in, so an interrupted unpack
    // never leaves a half-rootfs that looks usable.
    let staging = spec.dest.with_extension("unpacking");
    let _ = fs::remove_dir_all(&staging);
    extract(&spec.archive, &staging, progress)?;

    let _ = fs::remove_dir_all(&spec.dest);
    if let Some(parent) = spec.dest.parent() {
        fs::create_dir_all(parent).map_err(io(parent))?;
    }
    fs::rename(&staging, &spec.dest).map_err(io(&spec.dest))?;

    fs::write(spec.dest.join(MARKER), marker_for(spec)).map_err(io(spec.dest.join(MARKER)))?;
    let _ = fs::remove_file(&spec.archive);

    progress(Progress::Ready);
    Ok(spec.dest.clone())
}

fn marker_for(spec: &RootfsSpec) -> String {
    format!(
        "{}\n{}\n",
        spec.url,
        spec.sha256.as_deref().unwrap_or("no-checksum")
    )
}

/// Whether the rootfs on disk came from exactly this spec.
pub fn is_ready(spec: &RootfsSpec) -> bool {
    fs::read_to_string(spec.dest.join(MARKER))
        .map(|found| found == marker_for(spec))
        .unwrap_or(false)
}

async fn download_resumable(
    spec: &RootfsSpec,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<(), RootfsError> {
    use std::io::Write;

    use futures_util::StreamExt;

    if let Some(parent) = spec.archive.parent() {
        fs::create_dir_all(parent).map_err(io(parent))?;
    }

    let have = fs::metadata(&spec.archive).map(|m| m.len()).unwrap_or(0);

    let client = mc_core::http::client();
    let mut request = client.get(&spec.url);
    if have > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let response = request.send().await?.error_for_status()?;

    // 206 means the server honoured the range and we append. A 200 to a ranged
    // request means it ignored us and is sending the whole file, so the partial
    // file has to go rather than be appended to.
    let resuming = have > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if have > 0 && !resuming {
        fs::remove_file(&spec.archive).map_err(io(&spec.archive))?;
    }

    let already = if resuming { have } else { 0 };
    let total = response.content_length().map(|len| len + already);

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(resuming)
        .write(true)
        .truncate(!resuming)
        .open(&spec.archive)
        .map_err(io(&spec.archive))?;

    let mut downloaded = already;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).map_err(io(&spec.archive))?;
        downloaded += chunk.len() as u64;
        progress(Progress::Downloading { downloaded, total });
    }
    file.flush().map_err(io(&spec.archive))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, RootfsError> {
    let mut file = fs::File::open(path).map_err(io(path))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf).map_err(io(path))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Unpack a `.tar` or `.tar.gz` into `dest`.
///
/// Public because `tools/exec-probe` unpacks the same images, and the probe
/// exercising the real code is worth more than the probe having its own copy.
pub fn extract(
    archive: &Path,
    dest: &Path,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<u64, RootfsError> {
    fs::create_dir_all(dest).map_err(io(dest))?;
    let file = fs::File::open(archive).map_err(io(archive))?;

    // Sniff rather than trust the extension: Android's asset packer silently
    // gunzips `.gz` files and renames them, so the name lies.
    let mut magic = [0u8; 2];
    let gzipped = {
        let mut probe = fs::File::open(archive).map_err(io(archive))?;
        probe.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b]
    };

    if gzipped {
        unpack(tar::Archive::new(flate2::read::GzDecoder::new(file)), dest, progress)
    } else {
        unpack(tar::Archive::new(file), dest, progress)
    }
}

fn unpack<R: Read>(
    mut archive: tar::Archive<R>,
    dest: &Path,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<u64, RootfsError> {
    archive.set_preserve_permissions(true);
    // xattrs are not ours to set inside an app sandbox, and attempting them
    // turns every file into a failure.
    archive.set_unpack_xattrs(false);

    let mut entries = 0u64;
    // Device nodes, hardlinks and setuid bits in a distro tarball cannot be
    // recreated by an unprivileged app. That is expected - proot fakes them at
    // runtime - so skip what will not unpack instead of aborting the install.
    for entry in archive.entries().map_err(io(dest))? {
        let Ok(mut entry) = entry else { continue };
        if entry.unpack_in(dest).is_ok() {
            entries += 1;
            if entries.is_multiple_of(256) {
                progress(Progress::Extracting { entries });
            }
        }
    }
    progress(Progress::Extracting { entries });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop(_: Progress) {}

    fn spec(dir: &Path) -> RootfsSpec {
        RootfsSpec {
            url: "https://example.invalid/rootfs.tar".into(),
            sha256: Some("abc123".into()),
            dest: dir.join("rootfs"),
            archive: dir.join("rootfs.tar"),
        }
    }

    #[test]
    fn readiness_requires_a_marker_matching_this_exact_image() {
        let dir = std::env::temp_dir().join("mc-rootfs-marker");
        let _ = fs::remove_dir_all(&dir);
        let s = spec(&dir);
        fs::create_dir_all(&s.dest).unwrap();

        assert!(!is_ready(&s), "no marker yet");

        fs::write(s.dest.join(MARKER), marker_for(&s)).unwrap();
        assert!(is_ready(&s));

        // A different image must not be mistaken for this one.
        let other = RootfsSpec {
            sha256: Some("deadbeef".into()),
            ..s.clone()
        };
        assert!(!is_ready(&other), "a different checksum must invalidate");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_detects_gzip_from_the_bytes_not_the_filename() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("mc-rootfs-extract");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Build a tar containing one file, then gzip it - and name it ".tar",
        // the exact lie Android's asset packer tells.
        let mut builder = tar::Builder::new(Vec::new());
        let payload = b"inside the rootfs";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "bin/hello", &payload[..]).unwrap();
        let tar_bytes = builder.into_inner().unwrap();

        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        let gz_bytes = encoder.finish().unwrap();

        let lying_name = dir.join("rootfs.tar");
        fs::write(&lying_name, &gz_bytes).unwrap();

        let out = dir.join("out");
        let count = extract(&lying_name, &out, &noop).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(out.join("bin/hello")).unwrap(),
            "inside the rootfs"
        );

        // And the plain-tar path still works.
        let plain = dir.join("plain.tar");
        fs::write(&plain, &tar_bytes).unwrap();
        let out2 = dir.join("out2");
        assert_eq!(extract(&plain, &out2, &noop).unwrap(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_matches_a_known_value() {
        let dir = std::env::temp_dir().join("mc-rootfs-sha");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x");
        fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod alpine_tests {
    use super::*;

    #[test]
    fn the_pinned_alpine_url_is_well_formed() {
        let spec = RootfsSpec::alpine(Path::new("/data/files"));
        assert!(
            spec.url.starts_with("https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/"),
            "url was {:?}",
            spec.url
        );
        assert!(spec.url.ends_with(".tar.gz"), "url was {:?}", spec.url);
        assert!(!spec.url.contains(' '), "url has whitespace: {:?}", spec.url);
        assert!(spec.url.contains(ALPINE_VERSION));
        // A pin with no checksum is just trusting the network.
        assert_eq!(spec.sha256.as_ref().map(String::len), Some(64));
        assert_eq!(spec.dest, Path::new("/data/files/rootfs"));
    }
}
