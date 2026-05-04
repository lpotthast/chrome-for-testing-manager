use crate::ChromeForTestingManagerError;
use rootcause::{Report, option_ext::OptionExt, prelude::ResultExt};
use std::path::PathBuf;
use tokio::fs;

#[derive(Debug)]
pub(crate) struct CacheDir(PathBuf);

impl CacheDir {
    pub fn get_or_create() -> Result<Self, Report<ChromeForTestingManagerError>> {
        let project_dirs = directories::ProjectDirs::from("", "", "chrome-for-testing-manager")
            .context(ChromeForTestingManagerError::DetermineCacheDir)?;
        let cache_dir = project_dirs.cache_dir();
        Self::create_at(cache_dir.to_owned())
    }

    pub fn create_at(cache_dir: PathBuf) -> Result<Self, Report<ChromeForTestingManagerError>> {
        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir).context(
                ChromeForTestingManagerError::CreateCacheDir {
                    cache_dir: cache_dir.clone(),
                },
            )?;
        }
        Ok(Self(cache_dir))
    }

    pub fn path(&self) -> &PathBuf {
        &self.0
    }

    pub async fn clear(&self) -> Result<(), Report<ChromeForTestingManagerError>> {
        tracing::debug!("Clearing cache at {:?}...", self.path());
        fs::remove_dir_all(self.path()).await.context(
            ChromeForTestingManagerError::RemoveCacheDir {
                cache_dir: self.path().clone(),
            },
        )?;
        fs::create_dir_all(self.path()).await.context(
            ChromeForTestingManagerError::RecreateCacheDir {
                cache_dir: self.path().clone(),
            },
        )?;
        Ok(())
    }
}
