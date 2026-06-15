//! Shared application state handed to every handler.

use std::path::PathBuf;

use crate::db::Db;
use crate::disk::DiskConfig;
use crate::ga_config::GaConfig;

pub struct AppState {
    pub db: Db,
    /// Root for the regenerable on-disk caches: `<data_dir>/hist/...` and
    /// `<data_dir>/video/...`.
    pub data_dir: PathBuf,
    /// Disk-safety tunables (free-space floor + histogram-cache cap). The hist
    /// cache is bounded by these; see `disk.rs`.
    pub disk: DiskConfig,
    /// The GA "personality" for this world (mutation/immigrants/selection),
    /// read from env at boot; see `ga_config.rs`.
    pub ga: GaConfig,
}
