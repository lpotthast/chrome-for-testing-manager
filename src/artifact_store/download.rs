//! Cancellation-aware transfer of one Chrome for Testing artifact archive.
//!
//! Downloads stream into transaction-private staging files owned by the installation layer.

use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use rootcause::{bail, prelude::ResultExt};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Maximum number of bytes accepted for one artifact archive download. Real Chrome for Testing
/// archives stay well below this. The limit only stops a misbehaving server from filling the disk.
const MAX_DOWNLOAD_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Download one Chrome for Testing artifact archive to a transaction-private path.
///
/// The `timeout` bounds the whole request including its body and overrides the shared client's
/// default deadline. The caller owns the staging directory and is responsible for removing it on
/// every outcome.
#[tracing::instrument(skip(client, timeout, cancellation))]
pub(crate) async fn download_artifact_archive(
    client: &reqwest::Client,
    url: &str,
    archive_path: &Path,
    artifact: ChromeForTestingArtifact,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<()> {
    tracing::info!(%url, "Downloading artifact.");
    let download_error = || ChromeForTestingError::Download {
        artifact,
        url: url.to_owned(),
    };
    let write_error = || ChromeForTestingError::WriteDownloadFile {
        artifact,
        path: archive_path.to_owned(),
    };
    let mut response =
        crate::await_or_cancelled(cancellation, client.get(url).timeout(timeout).send())
            .await?
            .context_with(download_error)?
            .error_for_status()
            .context_with(download_error)?;

    let too_large_error = || ChromeForTestingError::DownloadTooLarge {
        artifact,
        url: url.to_owned(),
        max_size: MAX_DOWNLOAD_SIZE,
    };
    if let Some(content_length) = response.content_length() {
        if content_length > MAX_DOWNLOAD_SIZE {
            bail!(too_large_error());
        }
        #[allow(clippy::cast_precision_loss)]
        let content_length_mb = content_length as f64 / (1024.0 * 1024.0);
        tracing::info!(
            content_length,
            content_length_mb,
            "Artifact response received."
        );
    }

    let mut file = tokio::fs::File::create(archive_path)
        .await
        .context_with(write_error)?;
    let mut downloaded_size = 0_u64;

    loop {
        let chunk = crate::await_or_cancelled(cancellation, response.chunk())
            .await?
            .context_with(download_error)?;
        let Some(chunk) = chunk else {
            break;
        };

        downloaded_size = downloaded_size.saturating_add(chunk.len() as u64);
        if downloaded_size > MAX_DOWNLOAD_SIZE {
            bail!(too_large_error());
        }
        file.write_all(&chunk).await.context_with(write_error)?;
    }

    file.flush().await.context_with(write_error)?;
    crate::check_cancelled(cancellation)?;
    drop(file);
    tracing::info!(path = %archive_path.display(), "Artifact download complete.");
    Ok(())
}
