use std::fs;
use std::io::{self, Write};
use std::path::Path;

use atomicwrites::{AllowOverwrite, AtomicFile};

use crate::{ConfigError, PersistedConfig};

pub fn load_config(path: impl AsRef<Path>) -> Result<PersistedConfig, ConfigError> {
    let contents = fs::read_to_string(path)?;
    let persisted: PersistedConfig = toml::from_str(&contents)?;
    persisted.validate()?;
    Ok(persisted)
}

pub fn save_config(path: impl AsRef<Path>, persisted: &PersistedConfig) -> Result<(), ConfigError> {
    persisted.validate()?;

    // Serialize before opening the atomic temp file, so serialization failure
    // cannot disturb the last valid config.
    let contents = toml::to_string_pretty(persisted)?;
    atomic_write(path.as_ref(), |temporary| {
        temporary.write_all(contents.as_bytes())
    })?;

    Ok(())
}

fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    AtomicFile::new(path, AllowOverwrite)
        .write(write)
        .map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Write};

    use tempfile::tempdir;

    use super::atomic_write;

    #[test]
    fn interrupted_atomic_write_preserves_exact_previous_bytes() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("config.toml");
        let previous = b"last valid config bytes";
        fs::write(&target, previous).unwrap();

        let result = atomic_write(&target, |temporary| {
            temporary.write_all(b"partial replacement")?;
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "injected interruption",
            ))
        });

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(&target).unwrap(), previous);
    }
}
