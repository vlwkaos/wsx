// ^ [[wsx Architecture]] Durable daemon state publishes before in-memory intent becomes visible.
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(super) struct Loaded<T> {
    pub(super) state: T,
    pub(super) recovered_from_backup: bool,
}

#[derive(Debug)]
enum LoadFailure {
    Missing,
    Invalid(io::Error),
    Unsafe(io::Error),
}

pub(super) fn load<T>(path: &Path, validate: fn(&T) -> io::Result<()>) -> io::Result<Loaded<T>>
where
    T: DeserializeOwned + Default,
{
    match load_one(path, validate) {
        Ok(state) => Ok(Loaded {
            state,
            recovered_from_backup: false,
        }),
        Err(LoadFailure::Missing) => match load_one(&backup_path(path), validate) {
            Ok(state) => {
                eprintln!("wsxd recovered missing primary state from last-known-good backup");
                Ok(Loaded {
                    state,
                    recovered_from_backup: true,
                })
            }
            Err(LoadFailure::Missing) => Ok(Loaded {
                state: T::default(),
                recovered_from_backup: false,
            }),
            Err(LoadFailure::Invalid(error) | LoadFailure::Unsafe(error)) => Err(error),
        },
        Err(LoadFailure::Unsafe(error)) => Err(error),
        Err(LoadFailure::Invalid(primary_error)) => {
            let backup = match load_one(&backup_path(path), validate) {
                Ok(state) => state,
                Err(LoadFailure::Missing) => return Err(primary_error),
                Err(LoadFailure::Invalid(backup_error) | LoadFailure::Unsafe(backup_error)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "primary state is invalid ({primary_error}); backup is unusable ({backup_error})"
                        ),
                    ))
                }
            };
            let quarantine = quarantine_path(path);
            fs::rename(path, &quarantine)?;
            sync_parent(path)?;
            eprintln!(
                "wsxd quarantined invalid state at {} and loaded the last-known-good backup",
                quarantine.display()
            );
            Ok(Loaded {
                state: backup,
                recovered_from_backup: true,
            })
        }
    }
}

pub(super) fn save<T>(path: &Path, state: &T, validate: fn(&T) -> io::Result<()>) -> io::Result<()>
where
    T: Serialize + DeserializeOwned,
{
    validate(state)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state exceeds limit",
        ));
    }
    let temporary = path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.sync_all()?;

        let backup = backup_path(path);
        let had_primary = match load_one::<T>(path, validate) {
            Ok(_) => {
                fs::rename(path, &backup)?;
                sync_parent(path)?;
                true
            }
            Err(LoadFailure::Missing) => false,
            Err(LoadFailure::Invalid(error) | LoadFailure::Unsafe(error)) => return Err(error),
        };
        if let Err(error) = fs::rename(&temporary, path) {
            if had_primary {
                let _ = fs::rename(&backup, path);
            }
            return Err(error);
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        if !had_primary {
            let backup_temporary = path.with_extension(format!(
                "json.backup.tmp.{}.{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let backup_result = (|| {
                let mut backup_file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&backup_temporary)?;
                backup_file.write_all(&bytes)?;
                backup_file.sync_all()?;
                fs::rename(&backup_temporary, &backup)
            })();
            if backup_result.is_err() {
                let _ = fs::remove_file(&backup_temporary);
            }
            backup_result?;
        }
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn load_one<T>(path: &Path, validate: fn(&T) -> io::Result<()>) -> Result<T, LoadFailure>
where
    T: DeserializeOwned,
{
    let mut file = secure_file(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(LoadFailure::Unsafe)?;
    let state = serde_json::from_slice(&bytes)
        .map_err(|error| LoadFailure::Invalid(io::Error::new(io::ErrorKind::InvalidData, error)))?;
    validate(&state).map_err(LoadFailure::Invalid)?;
    Ok(state)
}

fn secure_file(path: &Path) -> Result<File, LoadFailure> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                LoadFailure::Missing
            } else {
                LoadFailure::Unsafe(error)
            }
        })?;
    let metadata = file.metadata().map_err(LoadFailure::Unsafe)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(LoadFailure::Unsafe(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe wsx state file",
        )));
    }
    if metadata.len() > MAX_STATE_BYTES {
        return Err(LoadFailure::Unsafe(io::Error::new(
            io::ErrorKind::InvalidData,
            "state file too large",
        )));
    }
    Ok(file)
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.backup")
}

fn quarantine_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    path.with_extension(format!("json.corrupt.{timestamp}"))
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct Fixture {
        value: u64,
    }

    fn valid(_fixture: &Fixture) -> io::Result<()> {
        Ok(())
    }

    fn path(name: &str) -> PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(1);
        let directory = std::env::current_dir()
            .unwrap()
            .join("target/wsx-state-store-tests");
        fs::create_dir_all(&directory).unwrap();
        directory.join(format!(
            "{name}-{}-{}.json",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn invalid_primary_falls_back_to_valid_backup_and_is_quarantined() {
        let path = path("fallback");
        save(&path, &Fixture { value: 1 }, valid).unwrap();
        save(&path, &Fixture { value: 2 }, valid).unwrap();
        fs::write(&path, b"not-json").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let loaded = load::<Fixture>(&path, valid).unwrap();
        assert_eq!(loaded.state, Fixture { value: 1 });
        assert!(loaded.recovered_from_backup);
        let prefix = path.file_stem().unwrap().to_string_lossy().to_string();
        let quarantined = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_name().to_string_lossy().starts_with(&prefix)
                    && entry.file_name().to_string_lossy().contains("corrupt")
            })
            .map(|entry| entry.path());
        assert!(quarantined.is_some());
        let _ = fs::remove_file(quarantined.unwrap());
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn save_never_rotates_an_invalid_primary_over_the_backup() {
        let path = path("preserve-backup");
        save(&path, &Fixture { value: 1 }, valid).unwrap();
        save(&path, &Fixture { value: 2 }, valid).unwrap();
        fs::write(&path, b"not-json").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(save(&path, &Fixture { value: 3 }, valid).is_err());
        assert_eq!(
            load_one::<Fixture>(&backup_path(&path), valid).unwrap(),
            Fixture { value: 1 }
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn unsafe_primary_does_not_fall_back() {
        let path = path("unsafe");
        save(&path, &Fixture { value: 1 }, valid).unwrap();
        save(&path, &Fixture { value: 2 }, valid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            load::<Fixture>(&path, valid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }
}
