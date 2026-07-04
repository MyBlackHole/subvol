use subvol_core::types::StorageError;

use crate::config::SubvolmountdConfig;

pub async fn init_dirs(config: &SubvolmountdConfig) -> Result<(), DaemonError> {
    let home = config.resolved_home_dir();
    tokio::fs::create_dir_all(&home).await?;
    tokio::fs::create_dir_all(config.pool_dir()).await?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("NBD error: {0}")]
    Nbd(#[from] subvol_nbd::NbdError),
}
