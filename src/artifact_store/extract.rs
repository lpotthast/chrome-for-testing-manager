//! Secure, bounded, and cancellation-aware ZIP extraction.
//!
//! Extraction runs in passes whose order carries the safety argument:
//!
//! 1. Directories and files are written. The staging directory is fresh and no symlink has been
//!    created yet, so no write can be redirected through a symlink.
//! 2. Sanitized permissions are applied, still before any symlink exists.
//! 3. Symlinks are created. They must be relative, must not be placed beneath another symlink, and
//!    must not replace an existing entry.
//! 4. Every symlink is resolved by the file system and must stay within the package subtree that
//!    will be published, because only that directory survives atomic publication. Dangling
//!    symlinks are rejected: they cannot be resolved, so their final target cannot be checked.
//!
//! Extraction cooperates with cancellation from its blocking worker.

use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use rootcause::{Report, bail, prelude::ResultExt};
use std::borrow::Cow;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::ZipArchive;
use zip::result::{ZipError, ZipResult};

const COPY_CHUNK_SIZE: usize = 64 * 1024;
/// Maximum total number of decompressed bytes accepted from one artifact archive.
const MAX_DECOMPRESSED_SIZE: u64 = 2 * 1024 * 1024 * 1024;
/// Maximum number of entries accepted from one artifact archive. Real Chrome for Testing archives
/// stay far below this. The limit only stops file-count bombs from exhausting inodes.
const MAX_ARCHIVE_ENTRIES: usize = 65_536;
/// Maximum length of a symlink target, in bytes. Longer targets are rejected before they are
/// buffered.
const MAX_SYMLINK_TARGET_LEN: u64 = 4096;

/// Extract an archive on a blocking worker and cooperatively observe cancellation.
///
/// The worker is always awaited. Cancellation therefore never leaves a detached extractor writing
/// into staging after its caller has started cleanup. Preserved symlinks must be relative and stay
/// within `package_root`, because only that directory survives atomic publication. `package_root`
/// must be a single normal path component.
pub(crate) async fn extract_zip(
    artifact: ChromeForTestingArtifact,
    archive_path: PathBuf,
    unpack_dir: PathBuf,
    package_root: PathBuf,
    cancellation: CancellationToken,
) -> Result<()> {
    let join_error = ChromeForTestingError::ExtractZip {
        artifact,
        path: archive_path.clone(),
        unpack_dir: unpack_dir.clone(),
    };
    let worker = tokio::task::spawn_blocking(move || {
        Extraction {
            artifact,
            archive_path: &archive_path,
            unpack_dir: &unpack_dir,
            cancellation: &cancellation,
            buffer: vec![0_u8; COPY_CHUNK_SIZE].into_boxed_slice(),
            extracted_size: 0,
        }
        .run(&package_root)
    });
    match worker.await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // Only possible while the runtime shuts down.
        Err(error) => Err(Report::new_sendsync(error).context(join_error)),
    }
}

/// A symlink entry, created only after every directory and file was written.
struct PendingSymlink {
    path: PathBuf,
    target: PathBuf,
}

/// One extraction run, its copy buffer, and its running decompressed-size total.
struct Extraction<'a> {
    artifact: ChromeForTestingArtifact,
    archive_path: &'a Path,
    unpack_dir: &'a Path,
    cancellation: &'a CancellationToken,
    buffer: Box<[u8]>,
    extracted_size: u64,
}

impl Extraction<'_> {
    fn error(&self) -> ChromeForTestingError {
        ChromeForTestingError::ExtractZip {
            artifact: self.artifact,
            path: self.archive_path.to_owned(),
            unpack_dir: self.unpack_dir.to_owned(),
        }
    }

    fn run(mut self, package_root: &Path) -> Result<()> {
        crate::check_cancelled(self.cancellation)?;
        fs::create_dir_all(self.unpack_dir).context_with(|| self.error())?;
        let base_path = self
            .unpack_dir
            .canonicalize()
            .context_with(|| self.error())?;
        let published_package = base_path.join(package_root);

        let archive_file =
            fs::File::open(self.archive_path).context(ChromeForTestingError::InvalidZip {
                artifact: self.artifact,
                path: self.archive_path.to_owned(),
            })?;
        let mut archive =
            ZipArchive::new(archive_file).context(ChromeForTestingError::InvalidZip {
                artifact: self.artifact,
                path: self.archive_path.to_owned(),
            })?;
        if archive.len() > MAX_ARCHIVE_ENTRIES {
            bail!(ChromeForTestingError::ZipTooManyEntries {
                artifact: self.artifact,
                path: self.archive_path.to_owned(),
                entries: archive.len() as u64,
                max_entries: MAX_ARCHIVE_ENTRIES as u64,
            });
        }

        let mut permissions = Vec::new();
        let mut symlinks = Vec::new();
        for index in 0..archive.len() {
            crate::check_cancelled(self.cancellation)?;
            let mut entry = archive.by_index(index).context_with(|| self.error())?;
            let path = safe_output_path(&base_path, &entry).context_with(|| self.error())?;
            if entry.is_symlink() {
                let target = self.read_symlink_target(&mut entry)?;
                symlinks.push(PendingSymlink { path, target });
                continue;
            }
            if entry.is_dir() {
                fs::create_dir_all(&path).context_with(|| self.error())?;
            } else {
                let parent = path.parent().ok_or_else(|| Report::new(self.error()))?;
                fs::create_dir_all(parent).context_with(|| self.error())?;
                let mut output = fs::File::create(&path).context_with(|| self.error())?;
                self.copy(&mut entry, &mut output)?;
            }
            if let Some(mode) = entry.unix_mode() {
                permissions.push((path, sanitize_mode(mode, entry.is_dir())));
            }
        }

        // No symlink exists yet, so permissions cannot be applied through one. Directories keep
        // owner write access, so symlinks can still be created beneath them.
        for (path, mode) in permissions.into_iter().rev() {
            crate::check_cancelled(self.cancellation)?;
            apply_unix_permissions(&path, mode).context_with(|| self.error())?;
        }
        for symlink in &symlinks {
            crate::check_cancelled(self.cancellation)?;
            create_package_symlink(symlink, &base_path, &published_package)
                .context_with(|| self.error())?;
        }
        if !symlinks.is_empty() {
            let package = published_package
                .canonicalize()
                .context_with(|| self.error())?;
            for symlink in &symlinks {
                ensure_symlink_resolves_inside(symlink, &package).context_with(|| self.error())?;
            }
        }
        Ok(())
    }

    fn read_symlink_target<R: Read>(&self, entry: &mut R) -> Result<PathBuf> {
        let mut target = Vec::new();
        entry
            .take(MAX_SYMLINK_TARGET_LEN + 1)
            .read_to_end(&mut target)
            .context_with(|| self.error())?;
        if target.len() as u64 > MAX_SYMLINK_TARGET_LEN {
            return Err(Report::new(invalid_archive("symlink target is too long")))
                .context_with(|| self.error());
        }
        let target = String::from_utf8(target).context_with(|| self.error())?;
        Ok(PathBuf::from(target))
    }

    /// Copy `input` to `output` in chunks, enforcing the decompressed-size limit and observing
    /// cancellation between chunks.
    fn copy<R: Read, W: Write>(&mut self, input: &mut R, output: &mut W) -> Result<()> {
        loop {
            crate::check_cancelled(self.cancellation)?;
            let count = input.read(&mut self.buffer).context_with(|| self.error())?;
            if count == 0 {
                return Ok(());
            }

            self.extracted_size = self.extracted_size.saturating_add(count as u64);
            if self.extracted_size > MAX_DECOMPRESSED_SIZE {
                bail!(ChromeForTestingError::ZipTooLarge {
                    artifact: self.artifact,
                    path: self.archive_path.to_owned(),
                    size: self.extracted_size,
                    max_size: MAX_DECOMPRESSED_SIZE,
                });
            }
            output
                .write_all(&self.buffer[..count])
                .context_with(|| self.error())?;
        }
    }
}

fn safe_output_path<R: Read>(
    base_path: &Path,
    entry: &zip::read::ZipFile<'_, R>,
) -> ZipResult<PathBuf> {
    let enclosed_name = entry
        .enclosed_name()
        .ok_or_else(|| invalid_archive("invalid archive entry path"))?;
    let outpath = normalize_path(&base_path.join(enclosed_name));
    if outpath.starts_with(base_path) {
        Ok(outpath)
    } else {
        Err(invalid_archive("archive entry path escapes destination"))
    }
}

/// Keep only the regular permission bits without group and other write access (no setuid,
/// setgid, or sticky bit), and always leave the owner able to read and write, so that staging and
/// cache cleanup can remove the entry. Nobody but the owner may replace a cached executable.
const fn sanitize_mode(mode: u32, is_dir: bool) -> u32 {
    let owner = if is_dir { 0o700 } else { 0o600 };
    (mode & 0o755) | owner
}

fn create_package_symlink(
    symlink: &PendingSymlink,
    base_path: &Path,
    published_package: &Path,
) -> ZipResult<()> {
    let PendingSymlink { path, target } = symlink;
    if target
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return Err(invalid_archive(
            "absolute symlink target would not survive package relocation",
        ));
    }
    if path == published_package || !path.starts_with(published_package) {
        return Err(invalid_archive("symlink is outside published package"));
    }
    ensure_no_symlink_ancestors(path, base_path)?;
    if fs::symlink_metadata(path).is_ok() {
        return Err(invalid_archive("symlink would replace an existing entry"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_archive("symlink has no parent directory"))?;
    fs::create_dir_all(parent)?;
    create_symlink(path, target)
}

/// Reject `path` if any directory between `base_path` and `path` is a symlink. Creating an entry
/// beneath a symlink would place it wherever that symlink points.
fn ensure_no_symlink_ancestors(path: &Path, base_path: &Path) -> ZipResult<()> {
    let relative_parent = path
        .parent()
        .and_then(|parent| parent.strip_prefix(base_path).ok())
        .ok_or_else(|| invalid_archive("archive entry path escapes destination"))?;
    let mut current = base_path.to_path_buf();
    for component in relative_parent.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid_archive("archive entry is placed beneath a symlink"));
            }
            Ok(_) => {}
            // Missing directories are created as real directories.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Require the file system's resolution of a created symlink to stay inside `package`, which must
/// be canonical.
///
/// A dangling symlink is rejected: a lexical check of its target cannot account for symlinks the
/// target path passes through, so where it would eventually point cannot be verified.
fn ensure_symlink_resolves_inside(symlink: &PendingSymlink, package: &Path) -> ZipResult<()> {
    let resolved = match fs::canonicalize(&symlink.path) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(invalid_archive("dangling symlink"));
        }
        Err(error) => return Err(error.into()),
    };
    if resolved.starts_with(package) {
        Ok(())
    } else {
        Err(invalid_archive("symlink target escapes published package"))
    }
}

#[cfg(unix)]
fn create_symlink(path: &Path, target: &Path) -> ZipResult<()> {
    std::os::unix::fs::symlink(target, path)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink(path: &Path, target: &Path) -> ZipResult<()> {
    let target_is_dir = path
        .parent()
        .is_some_and(|parent| parent.join(target).is_dir());
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, path)?;
    } else {
        std::os::windows::fs::symlink_file(target, path)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_path: &Path, _target: &Path) -> ZipResult<()> {
    Err(invalid_archive(
        "symlinks are not supported on this platform",
    ))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(_) | Component::Prefix(_) | Component::RootDir => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(unix)]
fn apply_unix_permissions(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn apply_unix_permissions(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

fn invalid_archive(message: &'static str) -> ZipError {
    ZipError::InvalidArchive(Cow::Borrowed(message))
}

#[cfg(test)]
mod tests {
    use super::extract_zip;
    use crate::test_support::TestDirectory;
    use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError};
    use assertr::prelude::*;
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::{Path, PathBuf};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    type ExtractResult = Result<PathBuf, rootcause::Report<ChromeForTestingError>>;

    /// Where [`extract`] unpacks, nested so that escapes above it can be detected.
    fn unpacked_dir(directory: &TestDirectory) -> PathBuf {
        directory.path().join("cache/version/unpacked")
    }

    /// Extract `archive` into [`unpacked_dir`] with package root `bundle`.
    async fn extract(
        directory: &TestDirectory,
        archive: Vec<u8>,
        cancellation: CancellationToken,
    ) -> ExtractResult {
        let archive_path = directory.path().join("archive.zip");
        fs::write(&archive_path, archive).expect("archive is written");
        let unpacked = unpacked_dir(directory);
        extract_zip(
            ChromeForTestingArtifact::Chrome,
            archive_path,
            unpacked.clone(),
            PathBuf::from("bundle"),
            cancellation,
        )
        .await
        .map(|()| unpacked)
    }

    async fn extract_uncancelled(directory: &TestDirectory, archive: Vec<u8>) -> ExtractResult {
        extract(directory, archive, CancellationToken::new()).await
    }

    fn archive(
        build: impl FnOnce(&mut ZipWriter<Cursor<Vec<u8>>>) -> zip::result::ZipResult<()>,
    ) -> zip::result::ZipResult<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        build(&mut writer)?;
        Ok(writer.finish()?.into_inner())
    }

    #[cfg(unix)]
    fn assert_not_written(directory: &TestDirectory, escaped: &[&str]) {
        for escaped in escaped {
            assert_that!(directory.path().join(escaped).exists())
                .with_detail_message(format!("{escaped} must not be written"))
                .is_false();
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn allows_relative_symlinks_inside_published_package() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-relative-symlink")?;
        let archive = archive(|writer| {
            writer.add_directory("bundle/target/", SimpleFileOptions::default())?;
            writer.start_file("bundle/target/file.txt", SimpleFileOptions::default())?;
            writer.write_all(b"ok")?;
            writer.add_symlink("bundle/current/", "target", SimpleFileOptions::default())
        })?;

        let unpacked = extract_uncancelled(&directory, archive).await?;

        assert_that!(fs::read_to_string(
            unpacked.join("bundle/current/file.txt")
        )?)
        .is_equal_to("ok");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_symlinks_to_discarded_staging_siblings() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-escaping-package-symlink")?;
        let archive = archive(|writer| {
            writer.start_file("payload/chrome", SimpleFileOptions::default())?;
            writer.write_all(b"browser")?;
            writer.add_symlink(
                "bundle/chrome",
                "../payload/chrome",
                SimpleFileOptions::default(),
            )
        })?;

        let error = extract_uncancelled(&directory, archive)
            .await
            .expect_err("published symlinks must not target discarded staging siblings");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::ExtractZip { .. }
        ))
        .is_true();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_dangling_symlinks_escaping_through_other_symlinks()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-dangling-symlink-escape")?;
        // `a` resolves to `bundle`. `b` lexically resolves into `bundle` as well, but the file
        // system follows `a` first and lands outside of it, at a path that does not exist (yet).
        let archive = archive(|writer| {
            writer.add_symlink("bundle/x/y/z/a", "../../..", SimpleFileOptions::default())?;
            writer.add_symlink(
                "bundle/x/y/z/b",
                "a/../../../../nonexistent",
                SimpleFileOptions::default(),
            )
        })?;

        assert_that!(extract_uncancelled(&directory, archive).await).is_err();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_symlink_chains_escaping_the_package() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-symlink-chain-escape")?;
        // `a` lexically and really resolves to `bundle`. `b` lexically resolves to `bundle` too,
        // but the file system follows `a` first and lands four levels above it.
        let archive = archive(|writer| {
            writer.add_symlink("bundle/x/y/z/a", "../../..", SimpleFileOptions::default())?;
            writer.add_symlink(
                "bundle/x/y/z/b",
                "a/../../../..",
                SimpleFileOptions::default(),
            )?;
            writer.start_file("bundle/x/y/z/b/evil", SimpleFileOptions::default())?;
            writer.write_all(b"escaped")?;
            Ok(())
        })?;

        assert_that!(extract_uncancelled(&directory, archive).await).is_err();
        assert_not_written(&directory, &["evil", "cache/evil", "cache/version/evil"]);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_symlinks_placed_beneath_symlinks() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-symlink-beneath-symlink")?;
        let archive = archive(|writer| {
            writer.add_directory("bundle/x/y/", SimpleFileOptions::default())?;
            writer.add_symlink("bundle/x/y/l", "..", SimpleFileOptions::default())?;
            writer.add_symlink("bundle/x/y/l/s", "../..", SimpleFileOptions::default())
        })?;

        assert_that!(extract_uncancelled(&directory, archive).await).is_err();
        let redirected = unpacked_dir(&directory).join("bundle/x/s");
        assert_that!(fs::symlink_metadata(redirected).is_err()).is_true();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_overlong_symlink_targets() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-overlong-symlink")?;
        let archive = archive(|writer| {
            writer.add_symlink(
                "bundle/link",
                "a/".repeat(4096),
                SimpleFileOptions::default(),
            )
        })?;

        assert_that!(extract_uncancelled(&directory, archive).await).is_err();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_archives_with_excessive_entry_counts() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-entry-count-limit")?;
        let archive = archive(|writer| {
            for index in 0..=super::MAX_ARCHIVE_ENTRIES {
                writer.start_file(format!("bundle/file-{index}"), SimpleFileOptions::default())?;
            }
            Ok(())
        })?;

        let error = extract_uncancelled(&directory, archive)
            .await
            .expect_err("archives beyond the entry-count limit must be rejected");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::ZipTooManyEntries { .. }
        ))
        .is_true();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn sanitizes_archive_permissions() -> Result<(), rootcause::Report> {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("extract-sanitized-permissions")?;
        let archive = archive(|writer| {
            writer.add_directory(
                "bundle/read-only/",
                SimpleFileOptions::default().unix_permissions(0o555),
            )?;
            writer.start_file(
                "bundle/read-only/setuid",
                SimpleFileOptions::default().unix_permissions(0o4755),
            )?;
            writer.write_all(b"binary")?;
            writer.start_file(
                "bundle/world-writable",
                SimpleFileOptions::default().unix_permissions(0o777),
            )?;
            writer.write_all(b"binary")?;
            Ok(())
        })?;

        let unpacked = extract_uncancelled(&directory, archive).await?;

        let mode = |path: &str| -> std::io::Result<u32> {
            Ok(fs::metadata(unpacked.join(path))?.permissions().mode() & 0o7777)
        };
        assert_that!(mode("bundle/read-only")?).is_equal_to(0o755);
        assert_that!(mode("bundle/read-only/setuid")?).is_equal_to(0o755);
        assert_that!(mode("bundle/world-writable")?).is_equal_to(0o755);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_cancelled_extraction_does_not_start() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-cancelled")?;
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert_that!(extract(&directory, many_entries_archive()?, cancellation).await).is_err();
        assert_that!(unpacked_dir(&directory).exists()).is_false();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_during_extraction_stops_before_more_entries_are_written()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-cooperative-cancellation")?;
        let bundle = unpacked_dir(&directory).join("bundle");
        let cancellation = CancellationToken::new();
        let cancel_after_first_entry = async {
            wait_until_exists(&bundle.join("file-0000.bin")).await;
            cancellation.cancel();
        };

        let (result, ()) = tokio::join!(
            extract(&directory, many_entries_archive()?, cancellation.clone()),
            cancel_after_first_entry,
        );
        let error = result.expect_err("in-flight extraction must observe cancellation");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        let entries_after_return = fs::read_dir(&bundle)?.count();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_that!(fs::read_dir(&bundle)?.count())
            .with_detail_message(
                "the awaited blocking worker must not keep extracting after return",
            )
            .is_equal_to(entries_after_return);
        assert_that!(entries_after_return).is_less_than(2_000);
        Ok(())
    }

    async fn wait_until_exists(path: &Path) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while tokio::fs::metadata(path).await.is_err() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("extraction produced its first entry");
    }

    fn many_entries_archive() -> zip::result::ZipResult<Vec<u8>> {
        let contents = [7_u8; 4 * 1024];
        archive(|writer| {
            for index in 0..2_000 {
                writer.start_file(
                    format!("bundle/file-{index:04}.bin"),
                    SimpleFileOptions::default(),
                )?;
                writer.write_all(&contents)?;
            }
            Ok(())
        })
    }
}
