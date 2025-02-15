use crate::mgr::Artifact;
use anyhow::Context;
use std::fs;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use zip_extensions::zip_extract;

pub(crate) async fn download_zip(
    client: &reqwest::Client,
    url: &str,
    download_dir: &Path,
    unpack_dir: &Path,
    artifact_type: Artifact, // TODO: add type to span. Drop this parameter.
) -> anyhow::Result<()> {
    // Initiate download.
    tracing::info!("Downloading {artifact_type:?} from {url:?}...");
    let response = client.get(url).send().await?;

    // Create new file for storage.
    let download_file_path = download_dir.join(format!(
        "{}.zip",
        match artifact_type {
            Artifact::Chrome => "chrome",
            Artifact::ChromeDriver => "chromedriver",
        }
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&download_file_path)
        .await
        .context("Failed to open new file to write downloaded zip into.")?;

    // Perform the download.
    write_file(&mut file, response).await?;
    tracing::info!("Completed {artifact_type:?} download");

    // TODO: validate download?

    // TODO: Check if zip.
    // TODO: Guard against zip-bomb.
    // TODO: Replace zip-extensions with better library?
    // Unpack the retrieved archive.
    tracing::info!("Extracting {artifact_type:?} to {unpack_dir:?}...");
    zip_extract(&download_file_path.to_owned(), &unpack_dir.to_owned())
        .context("Failed to extract zip file.")?;
    tracing::info!("Completed {artifact_type:?} extraction");

    // Remove downloaded archive.
    fs::remove_file(&download_file_path).context("Failed to remove zip file.")?;

    Ok(())
}

async fn write_file(
    file: &mut tokio::fs::File,
    mut response: reqwest::Response,
) -> anyhow::Result<()> {
    if let Some(content_length) = response.content_length() {
        tracing::info!("Content-Length: {}", content_length);
    }

    // TODO: Take note when download seems to hang (chunk() waiting for too long) and log such events.
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
    }

    file.flush().await?;

    anyhow::Ok(())
}
