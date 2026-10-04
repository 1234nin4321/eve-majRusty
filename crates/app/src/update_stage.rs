//! Platform-neutral half of the config dialog's in-place updater: download a release zip, verify it and unpack it to a
//! staging folder. The checks and the install script live in eve_maj_core::update; updater.rs owns the Windows/webui side.

use std::fmt;
use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use eve_maj_core::http_client::{FetchOptions, HttpClient};
use eve_maj_core::log::Scope;
use eve_maj_core::update::{
    is_safe_entry_name, validate_zip_entry_names, verify_download, Asset, StageError, MAX_DOWNLOAD_BYTES, REQUIRED_FILES,
};

const SLOG: Scope = Scope::new("update_stage");

#[derive(Debug)]
pub enum Error {
    Stage(StageError),
    Io(io::Error),
}

impl From<StageError> for Error {
    fn from(err: StageError) -> Self {
        Self::Stage(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stage(err) => err.fmt(f),
            Self::Io(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// The error code the config dialog's downloadUpdate handler reports for this failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Stage(StageError::DownloadFailed) => "download_failed",
            Self::Stage(StageError::SizeMismatch | StageError::ChecksumMismatch | StageError::UnsupportedDigest) => "verify_failed",
            Self::Stage(StageError::BadArchive | StageError::MissingRequiredFile) => "bad_archive",
            Self::Io(_) => "stage_failed",
        }
    }
}

/// Downloads `asset` into `work_dir`, verifies it, and extracts it to `<work_dir>/staged` (replacing any previous staging). Returns the staged directory path.
pub fn download_and_stage(asset: &Asset, work_dir: &Path) -> Result<PathBuf, Error> {
    let client = HttpClient::new();
    let body = client.fetch(&asset.url, &FetchOptions::default()).ok_or(StageError::DownloadFailed)?;
    if body.len() > MAX_DOWNLOAD_BYTES {
        return Err(StageError::DownloadFailed.into());
    }

    verify_download(&body, asset)?;
    stage_zip(&body, basename(&asset.name), work_dir)
}

/// Last component of `path`, treating both '/' and '\' as separators (Windows rules) and ignoring trailing ones.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// Writes the zip to `work_dir`, wipes and re-extracts `<work_dir>/staged`, and checks REQUIRED_FILES landed. Returns the staged path.
pub fn stage_zip(zip_bytes: &[u8], zip_name: &str, work_dir: &Path) -> Result<PathBuf, Error> {
    fs::create_dir_all(work_dir)?;

    // Checked before anything touches disk: the zip reader doesn't reject Windows drive paths like "C:/...", which would otherwise write outside staged/.
    validate_zip_entry_names(zip_bytes)?;

    fs::write(work_dir.join(zip_name), zip_bytes)?;

    let staged = work_dir.join("staged");
    match fs::remove_dir_all(&staged) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
        _ => {}
    }
    fs::create_dir_all(&staged)?;

    if let Err(err) = extract(zip_bytes, &staged) {
        SLOG.warn(format_args!("Failed to extract update zip: {err}"));
        return Err(StageError::BadArchive.into());
    }

    for name in REQUIRED_FILES {
        if fs::metadata(staged.join(name)).is_err() {
            SLOG.warn(format_args!("Update zip is missing required file '{name}'"));
            return Err(StageError::MissingRequiredFile.into());
        }
    }

    Ok(staged)
}

/// Unpacks every entry into `dest`, with backslashes taken as separators. Entry names were vetted by validate_zip_entry_names; each is re-checked against the reader's own view of the archive too.
fn extract(zip_bytes: &[u8], dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let raw = entry.name_raw();
        if !is_safe_entry_name(raw) {
            return Err(format!("unsafe entry name: {}", String::from_utf8_lossy(raw)).into());
        }
        let name = std::str::from_utf8(raw)?.replace('\\', "/");
        let path = dest.join(name.trim_end_matches('/'));
        if name.ends_with('/') {
            fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&path)?;
        // Reading to the end checks the entry's CRC-32.
        io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("eve-maj-update-stage-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            if name.ends_with('/') {
                writer.add_directory(*name, options).unwrap();
            } else {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn stage_zip_extracts_and_replaces_previous_staging() {
        let tmp = TempDir::new("ok");
        let stale = tmp.0.join("staged").join("stale.txt");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, b"old").unwrap();

        let zip = build_zip(&[
            ("eve-maj-preview.exe", b"main"),
            ("config.exe", b"config"),
            ("docs/", b""),
            ("lib\\nested\\file.txt", b"nested"),
        ]);
        let staged = stage_zip(&zip, "release.zip", &tmp.0).unwrap();
        assert_eq!(staged, tmp.0.join("staged"));
        assert_eq!(fs::read(tmp.0.join("release.zip")).unwrap(), zip);
        assert_eq!(fs::read(staged.join("config.exe")).unwrap(), b"config");
        assert_eq!(fs::read(staged.join("lib/nested/file.txt")).unwrap(), b"nested");
        assert!(staged.join("docs").is_dir());
        assert!(!stale.exists());
    }

    #[test]
    fn stage_zip_rejects_missing_files_and_unsafe_names() {
        let tmp = TempDir::new("bad");
        let zip = build_zip(&[("config.exe", b"config")]);
        assert!(matches!(stage_zip(&zip, "a.zip", &tmp.0), Err(Error::Stage(StageError::MissingRequiredFile))));

        let zip = build_zip(&[("eve-maj-preview.exe", b"m"), ("config.exe", b"c"), ("profiles/x.json", b"{}")]);
        assert!(matches!(stage_zip(&zip, "b.zip", &tmp.0), Err(Error::Stage(StageError::BadArchive))));
        assert!(!tmp.0.join("b.zip").exists());

        assert!(matches!(stage_zip(b"garbage that is long enough..", "c.zip", &tmp.0), Err(Error::Stage(StageError::BadArchive))));
    }

    #[test]
    fn basename_and_error_codes() {
        assert_eq!(basename("dir\\sub/file.zip"), "file.zip");
        assert_eq!(basename("file.zip"), "file.zip");
        assert_eq!(basename("a/b/"), "b");
        assert_eq!(Error::Stage(StageError::DownloadFailed).code(), "download_failed");
        assert_eq!(Error::Stage(StageError::ChecksumMismatch).code(), "verify_failed");
        assert_eq!(Error::Stage(StageError::MissingRequiredFile).code(), "bad_archive");
        assert_eq!(Error::Io(io::Error::other("x")).code(), "stage_failed");
    }
}
