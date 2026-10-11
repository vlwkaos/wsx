//! Per-device pairing. The CLI and the server share one owner-only file, so a
//! revoke takes effect on the next request without restarting the server.
//! ^ Only SHA-256 digests of codes and credentials are stored. Both are random
//! and high-entropy, so no password-stretching function is needed.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_DEVICES: usize = 16;
const MAX_PAIRINGS: usize = 4;
const PAIRING_SECONDS: u64 = 600;
// No 0/O, 1/I/L: the code is typed by hand on a phone.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const CODE_LENGTH: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    secret_sha256: String,
    pub created_unix: u64,
}

#[derive(Serialize, Deserialize)]
struct Pairing {
    code_sha256: String,
    expires_unix: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    #[serde(default)]
    devices: Vec<Device>,
    #[serde(default)]
    pairings: Vec<Pairing>,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .context("cannot read system randomness")?;
    Ok(bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}

fn same(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// Create the owner-only state directory used by every per-site store.
pub fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("cannot create {}", path.display()))
}

/// Replace a private file without leaving a partly written one behind.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

pub struct Devices {
    dir: PathBuf,
}

impl Devices {
    pub fn open(state_dir: &Path) -> Result<Self> {
        private_dir(state_dir)?;
        Ok(Self {
            dir: state_dir.to_owned(),
        })
    }

    /// Run one read-modify-write under an exclusive lock shared with the CLI.
    fn with_store<T>(&self, change: impl FnOnce(&mut Store) -> Result<(T, bool)>) -> Result<T> {
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.dir.join("devices.lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
            "cannot lock the device store"
        );
        let path = self.dir.join("devices.json");
        let mut store = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("device store is corrupt")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(error) => return Err(error.into()),
        };
        let now = now_unix();
        let before = store.pairings.len();
        store.pairings.retain(|pairing| pairing.expires_unix > now);
        let (value, changed) = change(&mut store)?;
        if changed || store.pairings.len() != before {
            write_private(&path, &serde_json::to_vec_pretty(&store)?)?;
        }
        Ok(value)
    }

    pub fn create_pairing(&self) -> Result<String> {
        let random: [u8; CODE_LENGTH] = random_bytes()?;
        // 248 = 8 * 31: reject the tail so every symbol is equally likely.
        let mut code = String::new();
        let mut pool = random.to_vec();
        while code.len() < CODE_LENGTH {
            let byte = match pool.pop() {
                Some(byte) => byte,
                None => {
                    pool = random_bytes::<CODE_LENGTH>()?.to_vec();
                    continue;
                }
            };
            if byte < 248 {
                code.push(CODE_ALPHABET[usize::from(byte) % CODE_ALPHABET.len()] as char);
            }
        }
        self.with_store(|store| {
            ensure!(
                store.pairings.len() < MAX_PAIRINGS,
                "too many unused pairing codes; wait for one to expire"
            );
            ensure!(
                store.devices.len() < MAX_DEVICES,
                "device limit reached; revoke one first"
            );
            store.pairings.push(Pairing {
                code_sha256: digest(&code),
                expires_unix: now_unix() + PAIRING_SECONDS,
            });
            Ok(((), true))
        })?;
        Ok(format!("{}-{}", &code[..5], &code[5..]))
    }

    /// Exchange a one-time code for a device credential. Returns the cookie value.
    pub fn redeem(&self, code: &str, name: &str) -> Result<Option<String>> {
        let code: String = code
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_uppercase())
            .collect();
        if code.len() != CODE_LENGTH {
            return Ok(None);
        }
        let name: String = name
            .chars()
            .filter(|character| !character.is_control())
            .take(40)
            .collect();
        let name = if name.trim().is_empty() {
            "device".to_owned()
        } else {
            name.trim().to_owned()
        };
        let wanted = digest(&code);
        let id = hex(&random_bytes::<4>()?);
        let secret = hex(&random_bytes::<32>()?);
        self.with_store(|store| {
            let Some(index) = store
                .pairings
                .iter()
                .position(|pairing| same(&pairing.code_sha256, &wanted))
            else {
                return Ok((None, false));
            };
            store.pairings.remove(index);
            if store.devices.len() >= MAX_DEVICES {
                bail!("device limit reached");
            }
            store.devices.push(Device {
                id: id.clone(),
                name,
                secret_sha256: digest(&secret),
                created_unix: now_unix(),
            });
            Ok((Some(format!("{id}.{secret}")), true))
        })
    }

    pub fn authenticate(&self, credential: &str) -> Result<Option<Device>> {
        let Some((id, secret)) = credential.split_once('.') else {
            return Ok(None);
        };
        if id.len() != 8 || secret.len() != 64 {
            return Ok(None);
        }
        let wanted = digest(secret);
        self.with_store(|store| {
            Ok((
                store
                    .devices
                    .iter()
                    .find(|device| device.id == id && same(&device.secret_sha256, &wanted))
                    .cloned(),
                false,
            ))
        })
    }

    pub fn list(&self) -> Result<Vec<Device>> {
        self.with_store(|store| Ok((store.devices.clone(), false)))
    }

    pub fn revoke(&self, id: &str) -> Result<bool> {
        self.with_store(|store| {
            let before = store.devices.len();
            store.devices.retain(|device| device.id != id);
            let removed = store.devices.len() != before;
            Ok((removed, removed))
        })
    }
}
