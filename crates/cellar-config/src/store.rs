use std::fs;
use std::io::Write;
use std::path::Path;

use atomicwrites::{AllowOverwrite, AtomicFile};

use crate::{ConfigError, PersistedConfig};

pub fn load_config(path: impl AsRef<Path>) -> Result<PersistedConfig, ConfigError> {
    let contents = fs::read_to_string(path)?;
    let persisted: PersistedConfig = toml::from_str(&contents)?;
    persisted.config.validate()?;
    Ok(persisted)
}

pub fn save_config(path: impl AsRef<Path>, persisted: &PersistedConfig) -> Result<(), ConfigError> {
    persisted.config.validate()?;

    // Serialize before opening the atomic temp file, so serialization failure
    // cannot disturb the last valid config.
    let contents = toml::to_string_pretty(persisted)?;
    let file = AtomicFile::new(path, AllowOverwrite);
    file.write(|temporary| temporary.write_all(contents.as_bytes()))
        .map_err(std::io::Error::from)?;

    Ok(())
}
