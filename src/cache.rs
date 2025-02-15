use std::path::PathBuf;
use tokio::fs;

#[derive(Debug)]
pub(crate) struct CacheDir(PathBuf);

impl CacheDir {
    pub fn get_or_create() -> Self {
        let project_dirs = directories::ProjectDirs::from("", "", "chromedriver-manager").unwrap();

        let cache_dir = project_dirs.cache_dir();
        if !cache_dir.exists() {
            std::fs::create_dir_all(cache_dir).unwrap();
        }

        Self(cache_dir.to_owned())
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
