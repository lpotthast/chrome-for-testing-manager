//! Secure, bounded, and cancellation-aware ZIP extraction.
//!
//! Extraction confines output and symlinks to the package subtree that will be published, applies
//! permissions after validation, and cooperates with cancellation from blocking workers.

use crate::{CancellationToken, ChromeForTestingError, Result};
use rootcause::{Report, bail, prelude::ResultExt};
use std::borrow::Cow;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::ZipArchive;
use zip::result::ZipError;

const COPY_CHUNK_SIZE: usize = 64 * 1024;
/// Maximum total number of decompressed bytes accepted from one artifact archive.
const MAX_DECOMPRESSED_SIZE: u64 = 2 * 1024 * 1024 * 1024;
/// Maximum number of entries accepted from one artifact archive. Real Chrome for Testing archives
/// stay far below this; the limit only stops file-count bombs from exhausting inodes.
const MAX_ARCHIVE_ENTRIES: usize = 65_536;

/// Extract an archive on a blocking worker and cooperatively observe cancellation.
///
/// The worker is always awaited. Cancellation therefore never leaves a detached extractor writing
/// into staging after its caller has started cleanup. Preserved symlinks must be relative and stay
/// within `package_root`, because only that directory survives atomic publication.
pub(crate) async fn extract_zip(
    archive_path: PathBuf,
    unpack_dir: PathBuf,
    package_root: PathBuf,
    cancellation: CancellationToken,
) -> Result<()> {
    let join_archive_path = archive_path.clone();
    let join_unpack_dir = unpack_dir.clone();
    tokio::task::spawn_blocking(move || {
        extract_zip_blocking(&archive_path, &unpack_dir, &package_root, &cancellation)
    })
    .await
    .context(ChromeForTestingError::ExtractZip {
        path: join_archive_path,
        unpack_dir: join_unpack_dir,
    })?
}

fn extract_zip_blocking(
    archive_path: &Path,
    unpack_dir: &Path,
    package_root: &Path,
    cancellation: &CancellationToken,
) -> Result<()> {
    let extract_error = || ChromeForTestingError::ExtractZip {
        path: archive_path.to_owned(),
        unpack_dir: unpack_dir.to_owned(),
    };

    crate::check_cancelled(cancellation)?;
    fs::create_dir_all(unpack_dir).context_with(extract_error)?;
    let base_path = normalize_path(&unpack_dir.canonicalize().context_with(extract_error)?);
    let published_package = normalize_path(&base_path.join(package_root));
    if !published_package.starts_with(&base_path) {
        bail!(ChromeForTestingError::InvalidPackageExecutablePath {
            path: package_root.to_owned(),
        });
    }

    let archive_file = fs::File::open(archive_path).context(ChromeForTestingError::InvalidZip {
        path: archive_path.to_owned(),
    })?;
    let mut archive = ZipArchive::new(archive_file).context(ChromeForTestingError::InvalidZip {
        path: archive_path.to_owned(),
    })?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!(ChromeForTestingError::ZipTooManyEntries {
            path: archive_path.to_owned(),
            entries: archive.len() as u64,
            max_entries: MAX_ARCHIVE_ENTRIES as u64,
        });
    }

    let mut extracted_size = 0_u64;
    let mut deferred_permissions = Vec::new();
    for index in 0..archive.len() {
        crate::check_cancelled(cancellation)?;
        let mut entry = archive.by_index(index).context_with(extract_error)?;
        let outpath = safe_output_path(&base_path, &entry).context_with(extract_error)?;
        ensure_existing_symlinks_stay_inside_destination(&outpath, &base_path)
            .context_with(extract_error)?;

        let parent = outpath
            .parent()
            .ok_or_else(|| Report::new(extract_error()))?;
        fs::create_dir_all(parent).context_with(extract_error)?;

        if entry.is_symlink() {
            extract_symlink(
                &mut entry,
                &outpath,
                &published_package,
                archive_path,
                unpack_dir,
                cancellation,
                &mut extracted_size,
            )?;
        } else if entry.is_dir() {
            fs::create_dir_all(&outpath).context_with(extract_error)?;
            deferred_permissions.push((outpath, entry.unix_mode()));
        } else {
            let mut output = fs::File::create(&outpath).context_with(extract_error)?;
            copy_cooperatively(
                &mut entry,
                &mut output,
                archive_path,
                unpack_dir,
                cancellation,
                &mut extracted_size,
            )?;
            deferred_permissions.push((outpath, entry.unix_mode()));
        }
    }

    apply_deferred_permissions(deferred_permissions, archive_path, unpack_dir, cancellation)
}

/// Apply permissions only after every entry is present. In particular, a read-only directory
/// must not prevent later entries from being created beneath it.
fn apply_deferred_permissions(
    deferred_permissions: Vec<(PathBuf, Option<u32>)>,
    archive_path: &Path,
    unpack_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<()> {
    let extract_error = || ChromeForTestingError::ExtractZip {
        path: archive_path.to_owned(),
        unpack_dir: unpack_dir.to_owned(),
    };
    for (path, mode) in deferred_permissions.into_iter().rev() {
        crate::check_cancelled(cancellation)?;
        apply_unix_permissions(&path, mode).context_with(extract_error)?;
    }
    Ok(())
}

fn copy_cooperatively<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    archive_path: &Path,
    unpack_dir: &Path,
    cancellation: &CancellationToken,
    extracted_size: &mut u64,
) -> Result<()> {
    let extract_error = || ChromeForTestingError::ExtractZip {
        path: archive_path.to_owned(),
        unpack_dir: unpack_dir.to_owned(),
    };
    let mut buffer = vec![0_u8; COPY_CHUNK_SIZE].into_boxed_slice();
    loop {
        crate::check_cancelled(cancellation)?;
        let count = input.read(&mut buffer).context_with(extract_error)?;
        if count == 0 {
            return Ok(());
        }

        *extracted_size = extracted_size.saturating_add(count as u64);
        if *extracted_size > MAX_DECOMPRESSED_SIZE {
            bail!(ChromeForTestingError::ZipTooLarge {
                path: archive_path.to_owned(),
                size: *extracted_size,
                max_size: MAX_DECOMPRESSED_SIZE,
            });
        }
        output
            .write_all(&buffer[..count])
            .context_with(extract_error)?;
    }
}

fn safe_output_path<R: Read>(
    base_path: &Path,
    entry: &zip::read::ZipFile<'_, R>,
) -> zip::result::ZipResult<PathBuf> {
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

fn extract_symlink<R: Read>(
    entry: &mut zip::read::ZipFile<'_, R>,
    outpath: &Path,
    published_package: &Path,
    archive_path: &Path,
    unpack_dir: &Path,
    cancellation: &CancellationToken,
    extracted_size: &mut u64,
) -> Result<()> {
    let extract_error = || ChromeForTestingError::ExtractZip {
        path: archive_path.to_owned(),
        unpack_dir: unpack_dir.to_owned(),
    };
    let mut target = Vec::new();
    copy_cooperatively(
        entry,
        &mut target,
        archive_path,
        unpack_dir,
        cancellation,
        extracted_size,
    )?;
    let target = std::str::from_utf8(&target).context_with(extract_error)?;
    ensure_symlink_stays_inside_published_package(outpath, target, published_package)
        .context_with(extract_error)?;
    remove_existing_path(outpath).context_with(extract_error)?;
    create_symlink(outpath, target).context_with(extract_error)?;
    Ok(())
}

fn ensure_symlink_stays_inside_published_package(
    outpath: &Path,
    target: &str,
    published_package: &Path,
) -> zip::result::ZipResult<()> {
    let target = Path::new(target);
    if target.is_absolute() {
        return Err(invalid_archive(
            "absolute symlink target would not survive package relocation",
        ));
    }
    if !outpath.starts_with(published_package) {
        return Err(invalid_archive("symlink is outside published package"));
    }

    let resolved_target =
        normalize_path(&outpath.parent().unwrap_or(published_package).join(target));
    if !resolved_target.starts_with(published_package) {
        return Err(invalid_archive("symlink target escapes published package"));
    }
    Ok(())
}

fn ensure_existing_symlinks_stay_inside_destination(
    outpath: &Path,
    base_path: &Path,
) -> zip::result::ZipResult<()> {
    let relative_path = outpath
        .strip_prefix(base_path)
        .map_err(|_| invalid_archive("archive entry path escapes destination"))?;
    let mut current = base_path.to_path_buf();

    for component in relative_path.components() {
        current.push(component.as_os_str());
        let mut followed_symlinks = 0;
        loop {
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            };
            if !metadata.file_type().is_symlink() {
                break;
            }
            if followed_symlinks == 16 {
                return Err(invalid_archive("existing symlink chain is too deep"));
            }
            followed_symlinks += 1;

            let target = fs::read_link(&current)?;
            current = if target.is_absolute() {
                normalize_path(&target)
            } else {
                normalize_path(&current.parent().unwrap_or(base_path).join(target))
            };
            if !current.starts_with(base_path) {
                return Err(invalid_archive("existing symlink escapes destination"));
            }
        }
    }
    Ok(())
}

fn remove_existing_path(path: &Path) -> zip::result::ZipResult<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(outpath: &Path, target: &str) -> zip::result::ZipResult<()> {
    std::os::unix::fs::symlink(Path::new(target), outpath)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink(outpath: &Path, target: &str) -> zip::result::ZipResult<()> {
    std::os::windows::fs::symlink_file(Path::new(target), outpath)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(outpath: &Path, target: &str) -> zip::result::ZipResult<()> {
    fs::write(outpath, target)?;
    Ok(())
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
fn apply_unix_permissions(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_unix_permissions(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
    Ok(())
}

fn invalid_archive(message: &'static str) -> ZipError {
    ZipError::InvalidArchive(Cow::Borrowed(message))
}

#[cfg(test)]
mod tests {
    use super::extract_zip;
    use crate::CancellationToken;
    use crate::test_support::TestDirectory;
    use assertr::prelude::*;
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::{Path, PathBuf};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn allows_relative_symlinks_inside_published_package() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-relative-symlink")?;
        let archive = directory.path().join("archive.zip");
        write_archive(&archive, true)?;

        extract_zip(
            archive,
            directory.path().join("unpacked"),
            PathBuf::from("bundle"),
            CancellationToken::new(),
        )
        .await?;

        assert_that!(fs::read_to_string(
            directory.path().join("unpacked/bundle/current/file.txt")
        )?)
        .is_equal_to("ok");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_symlinks_to_discarded_staging_siblings() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-escaping-package-symlink")?;
        let archive = directory.path().join("archive.zip");
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer.start_file("payload/chrome", SimpleFileOptions::default())?;
        writer.write_all(b"browser")?;
        writer.add_symlink(
            "bundle/chrome",
            "../payload/chrome",
            SimpleFileOptions::default(),
        )?;
        fs::write(&archive, writer.finish()?.into_inner())?;

        let error = extract_zip(
            archive,
            directory.path().join("unpacked"),
            PathBuf::from("bundle"),
            CancellationToken::new(),
        )
        .await
        .expect_err("published symlinks must not target discarded staging siblings");
        assert_that!(matches!(
            error.current_context(),
            crate::ChromeForTestingError::ExtractZip { .. }
        ))
        .is_true();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_archives_with_excessive_entry_counts() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-entry-count-limit")?;
        let archive = directory.path().join("archive.zip");
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..=super::MAX_ARCHIVE_ENTRIES {
            writer.start_file(format!("bundle/file-{index}"), SimpleFileOptions::default())?;
        }
        fs::write(&archive, writer.finish()?.into_inner())?;

        let error = extract_zip(
            archive,
            directory.path().join("unpacked"),
            PathBuf::from("bundle"),
            CancellationToken::new(),
        )
        .await
        .expect_err("archives beyond the entry-count limit must be rejected");
        assert_that!(matches!(
            error.current_context(),
            crate::ChromeForTestingError::ZipTooManyEntries { .. }
        ))
        .is_true();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_cancelled_extraction_does_not_start() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-cancelled")?;
        let archive = directory.path().join("archive.zip");
        write_archive(&archive, false)?;
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert_that!(
            extract_zip(
                archive,
                directory.path().join("unpacked"),
                PathBuf::from("bundle"),
                cancellation,
            )
            .await
        )
        .is_err();
        assert_that!(tokio::fs::metadata(directory.path().join("unpacked")).await).is_err();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_during_extraction_stops_before_more_entries_are_written()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("extract-cooperative-cancellation")?;
        let archive = directory.path().join("archive.zip");
        write_many_entries_archive(&archive)?;
        let unpacked = directory.path().join("unpacked");
        let cancellation = CancellationToken::new();
        let cancel_after_first_entry = async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while tokio::fs::metadata(unpacked.join("bundle/file-0000.bin"))
                    .await
                    .is_err()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("extraction produced its first entry");
            cancellation.cancel();
        };

        let (result, ()) = tokio::join!(
            extract_zip(
                archive,
                unpacked.clone(),
                PathBuf::from("bundle"),
                cancellation.clone(),
            ),
            cancel_after_first_entry,
        );
        let error = result.expect_err("in-flight extraction must observe cancellation");
        assert_that!(matches!(
            error.current_context(),
            crate::ChromeForTestingError::Cancelled
        ))
        .is_true();
        let entries_after_return = fs::read_dir(unpacked.join("bundle"))?.count();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_that!(fs::read_dir(unpacked.join("bundle"))?.count())
            .with_detail_message(
                "the awaited blocking worker must not keep extracting after return",
            )
            .is_equal_to(entries_after_return);
        assert_that!(entries_after_return).is_less_than(2_000);
        Ok(())
    }

    fn write_archive(path: &Path, symlink: bool) -> zip::result::ZipResult<()> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer.add_directory("bundle/target/", SimpleFileOptions::default())?;
        writer.start_file("bundle/target/file.txt", SimpleFileOptions::default())?;
        writer.write_all(b"ok")?;
        if symlink {
            writer.add_symlink("bundle/current/", "target", SimpleFileOptions::default())?;
        }
        fs::write(path, writer.finish()?.into_inner())?;
        Ok(())
    }

    fn write_many_entries_archive(path: &Path) -> zip::result::ZipResult<()> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let contents = [7_u8; 4 * 1024];
        for index in 0..2_000 {
            writer.start_file(
                format!("bundle/file-{index:04}.bin"),
                SimpleFileOptions::default(),
            )?;
            writer.write_all(&contents)?;
        }
        fs::write(path, writer.finish()?.into_inner())?;
        Ok(())
    }
}
