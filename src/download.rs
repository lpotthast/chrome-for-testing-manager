use crate::{ChromeForTestingArtifact, ChromeForTestingManagerError};
use rootcause::{Report, bail, prelude::ResultExt};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use zip::ZipArchive;

const MAX_DECOMPRESSED_SIZE: u128 = 2 * 1024 * 1024 * 1024; // 2 GB
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_STALLS: u32 = 3;

#[tracing::instrument(skip(client))]
pub(crate) async fn download_zip(
    client: &reqwest::Client,
    url: &str,
    download_dir: &Path,
    unpack_dir: &Path,
    artifact: ChromeForTestingArtifact,
) -> Result<(), Report<ChromeForTestingManagerError>> {
    // Initiate download and validate HTTP response.
    tracing::info!("Downloading from {url:?}...");
    let response = client
        .get(url)
        .send()
        .await
        .context(ChromeForTestingManagerError::Download {
            artifact,
            url: url.to_owned(),
        })?
        .error_for_status()
        .context(ChromeForTestingManagerError::Download {
            artifact,
            url: url.to_owned(),
        })?;

    // Create new file for storage.
    let download_file_path = download_dir.join(format!("{artifact}.zip"));
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&download_file_path)
        .await
        .context(ChromeForTestingManagerError::CreateDownloadFile {
            artifact,
            path: download_file_path.clone(),
        })?;

    // Perform the download.
    write_file(&mut file, response, artifact).await?;
    tracing::info!("Download complete");

    // Open and validate the archive.
    let zip_file = fs::File::open(&download_file_path).context(
        ChromeForTestingManagerError::OpenDownloadedZip {
            path: download_file_path.clone(),
        },
    )?;
    let mut archive =
        ZipArchive::new(zip_file).context(ChromeForTestingManagerError::InvalidZip {
            path: download_file_path.clone(),
        })?;

    // Guard against zip bombs.
    if let Some(size) = archive.decompressed_size()
        && size > MAX_DECOMPRESSED_SIZE
    {
        bail!(ChromeForTestingManagerError::ZipTooLarge {
            path: download_file_path.clone(),
            size,
            max_size: MAX_DECOMPRESSED_SIZE,
        });
    }

    // Extract.
    tracing::info!("Extracting to {unpack_dir:?}...");
    archive
        .extract(unpack_dir)
        .context(ChromeForTestingManagerError::ExtractZip {
            path: download_file_path.clone(),
            unpack_dir: unpack_dir.to_owned(),
        })?;
    tracing::info!("Extraction complete");

    // Remove downloaded archive.
    fs::remove_file(&download_file_path).context(
        ChromeForTestingManagerError::RemoveDownloadedZip {
            path: download_file_path,
        },
    )?;

    Ok(())
}

async fn write_file(
    file: &mut tokio::fs::File,
    mut response: reqwest::Response,
    artifact: ChromeForTestingArtifact,
) -> Result<(), Report<ChromeForTestingManagerError>> {
    if let Some(content_length) = response.content_length() {
        #[allow(clippy::cast_precision_loss)] // Display-only; precision loss is irrelevant.
        let content_length_mb = content_length as f64 / (1024.0 * 1024.0);
        tracing::info!("Content-Length: {content_length} ({content_length_mb:.2} MB)");
    }

    let mut consecutive_stalls: u32 = 0;

    loop {
        match timeout(CHUNK_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                consecutive_stalls = 0;
                file.write_all(&chunk)
                    .await
                    .context(ChromeForTestingManagerError::WriteDownloadFile { artifact })?;
            }
            Ok(Ok(None)) => break,
            Ok(Err(err)) => {
                return Err(
                    Report::new(err).context(ChromeForTestingManagerError::Download {
                        artifact,
                        url: response.url().to_string(),
                    }),
                );
            }
            Err(_elapsed) => {
                consecutive_stalls += 1;
                tracing::warn!(
                    consecutive_stalls,
                    "Download stalled (no data received for {CHUNK_TIMEOUT:?})."
                );
                if consecutive_stalls >= MAX_CONSECUTIVE_STALLS {
                    bail!(ChromeForTestingManagerError::DownloadStalled {
                        artifact,
                        consecutive_stalls,
                        chunk_timeout: CHUNK_TIMEOUT,
                    });
                }
            }
        }
    }

    file.flush()
        .await
        .context(ChromeForTestingManagerError::FlushDownloadFile { artifact })?;

    Ok(())
}
