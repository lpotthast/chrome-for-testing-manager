use anyhow::Context;
use std::path::PathBuf;
use tokio::fs;

#[derive(Debug)]
pub(crate) struct CacheDir(PathBuf);

impl CacheDir {
    pub fn get_or_create() -> anyhow::Result<Self> {
        let project_dirs = directories::ProjectDirs::from("", "", "chrome-for-testing-manager")
            .context("Failed to determine cache directory (is $HOME set?)")?;

        let cache_dir = project_dirs.cache_dir();
        if !cache_dir.exists() {
            std::fs::create_dir_all(cache_dir).context("Failed to create cache directory")?;
        }

        Ok(Self(cache_dir.to_owned()))
    }

    pub fn path(&self) -> &PathBuf {
        &self.0
    }

    pub async fn clear(&self) -> anyhow::Result<()> {
        tracing::info!("Clearing cache at {:?}...", self.path());
        fs::remove_dir_all(self.path()).await?;
        fs::create_dir_all(self.path()).await?;
        Ok(())
    }
}
