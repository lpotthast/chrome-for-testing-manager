use crate::mgr::Artifact;
use anyhow::Context;
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
    artifact: Artifact,
) -> anyhow::Result<()> {
    // Initiate download and validate HTTP response.
    tracing::info!("Downloading from {url:?}...");
    let response = client
        .get(url)
        .send()
        .await?
        .error_for_status()
        .context("Download failed with non-success HTTP status")?;

    // Create new file for storage.
    let download_file_path = download_dir.join(format!("{artifact}.zip"));
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&download_file_path)
        .await
        .context("Failed to open new file to write downloaded zip into.")?;

    // Perform the download.
    write_file(&mut file, response).await?;
    tracing::info!("Download complete");

    // Open and validate the archive.
    let zip_file =
        fs::File::open(&download_file_path).context("Failed to open downloaded zip file")?;
    let mut archive =
        ZipArchive::new(zip_file).context("Downloaded file is not a valid ZIP archive")?;

    // Guard against zip bombs.
    if let Some(size) = archive.decompressed_size() {
        anyhow::ensure!(
            size <= MAX_DECOMPRESSED_SIZE,
            "Archive decompressed size ({size} bytes) exceeds the {MAX_DECOMPRESSED_SIZE}-byte safety limit"
        );
    }

    // Extract.
    tracing::info!("Extracting to {unpack_dir:?}...");
    archive
        .extract(unpack_dir)
        .context("Failed to extract ZIP archive")?;
    tracing::info!("Extraction complete");

    // Remove downloaded archive.
    fs::remove_file(&download_file_path).context("Failed to remove zip file.")?;

    Ok(())
}

async fn write_file(
    file: &mut tokio::fs::File,
    mut response: reqwest::Response,
) -> anyhow::Result<()> {
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
                file.write_all(&chunk).await?;
            }
            Ok(Ok(None)) => break,
            Ok(Err(err)) => return Err(err.into()),
            Err(_elapsed) => {
                consecutive_stalls += 1;
                tracing::warn!(
                    consecutive_stalls,
                    "Download stalled (no data received for {CHUNK_TIMEOUT:?})."
                );
                if consecutive_stalls >= MAX_CONSECUTIVE_STALLS {
                    anyhow::bail!(
                        "Download timed out after {MAX_CONSECUTIVE_STALLS} consecutive \
                         stalls of {CHUNK_TIMEOUT:?} each."
                    );
                }
            }
        }
    }

    file.flush().await?;

    anyhow::Ok(())
}
