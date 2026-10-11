//! Per-site, content-addressed media store. See docs/web-client.md "Media".
//! ^ A URL names the SHA-256 of the bytes, so a browser may cache it as immutable.
//! An entry lives from its first upload until the TTL or quota removes it.
use crate::devices::{hex, now_unix, private_dir, random_bytes, write_private};
use crate::site::MediaLimits;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_TEXT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub hash: String,
    pub ext: String,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub created_unix: u64,
}

impl Entry {
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.hash, self.ext)
    }
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MediaError {
    TooLarge,
    UnsupportedType,
    Io,
}

pub struct Media {
    dir: PathBuf,
    limits: MediaLimits,
    index: Mutex<Vec<Entry>>,
}

/// One in-progress upload. Dropping it removes the partial file.
pub struct Upload {
    path: PathBuf,
    file: Option<File>,
    hasher: Sha256,
    size: u64,
    head: Vec<u8>,
    limit: u64,
}

impl Upload {
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), MediaError> {
        self.size += chunk.len() as u64;
        if self.size > self.limit {
            return Err(MediaError::TooLarge);
        }
        if self.head.len() < 64 {
            let take = (64 - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
        }
        self.hasher.update(chunk);
        self.file
            .as_mut()
            .ok_or(MediaError::Io)?
            .write_all(chunk)
            .map_err(|_| MediaError::Io)
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Media {
    pub fn open(state_dir: &Path, limits: MediaLimits) -> Result<Self> {
        let dir = state_dir.join("media");
        private_dir(&dir)?;
        let mut index: Vec<Entry> = fs::read(dir.join("index.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        // The index is the authority; drop rows whose file is gone and files with no row.
        index.retain(|entry| dir.join(entry.file_name()).is_file());
        for item in fs::read_dir(&dir)?.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            if name != "index.json" && !index.iter().any(|entry| entry.file_name() == name) {
                let _ = fs::remove_file(item.path());
            }
        }
        let media = Self {
            dir,
            limits,
            index: Mutex::new(index),
        };
        media.sweep();
        Ok(media)
    }

    pub fn path(&self, entry: &Entry) -> PathBuf {
        self.dir.join(entry.file_name())
    }

    pub fn ttl_seconds(&self) -> u64 {
        self.limits.ttl_seconds
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.limits.max_file_bytes
    }

    fn save(&self, index: &[Entry]) {
        if let Ok(bytes) = serde_json::to_vec(index) {
            let _ = write_private(&self.dir.join("index.json"), &bytes);
        }
    }

    fn remove(&self, index: &mut Vec<Entry>, position: usize) {
        let entry = index.remove(position);
        let _ = fs::remove_file(self.path(&entry));
    }

    /// Remove expired entries. Called by the server's sweeper and before each write.
    pub fn sweep(&self) -> usize {
        let mut index = self.index.lock().unwrap_or_else(|lock| lock.into_inner());
        let cutoff = now_unix().saturating_sub(self.limits.ttl_seconds);
        let before = index.len();
        while let Some(position) = index.iter().position(|entry| entry.created_unix <= cutoff) {
            self.remove(&mut index, position);
        }
        if index.len() != before {
            self.save(&index);
        }
        before - index.len()
    }

    pub fn begin(&self) -> Result<Upload, MediaError> {
        let nonce = random_bytes::<8>().map_err(|_| MediaError::Io)?;
        let path = self.dir.join(format!("upload-{}.part", hex(&nonce)));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|_| MediaError::Io)?;
        Ok(Upload {
            path,
            file: Some(file),
            hasher: Sha256::new(),
            size: 0,
            head: Vec::new(),
            limit: self.limits.max_file_bytes,
        })
    }

    pub fn finish(&self, mut upload: Upload, name: &str) -> Result<Entry, MediaError> {
        let file = upload.file.take().ok_or(MediaError::Io)?;
        file.sync_all().map_err(|_| MediaError::Io)?;
        drop(file);
        if upload.size == 0 {
            return Err(MediaError::UnsupportedType);
        }
        let name = clean_name(name);
        let (ext, mime) = match sniff(&upload.head) {
            Some(kind) => kind,
            None => {
                // No binary signature: accept only bounded, valid UTF-8 text.
                let text = upload.size <= MAX_TEXT_BYTES
                    && fs::read(&upload.path)
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                        .is_some_and(|text| !text.contains('\0'));
                if !text {
                    return Err(MediaError::UnsupportedType);
                }
                ("txt", "text/plain; charset=utf-8")
            }
        };
        let hash = hex(&upload.hasher.clone().finalize());
        self.sweep();
        let mut index = self.index.lock().unwrap_or_else(|lock| lock.into_inner());
        // The same bytes again: keep the entry and restart its TTL, so a file that
        // was just attached is not removed moments later.
        if let Some(existing) = index.iter_mut().find(|entry| entry.hash == hash) {
            existing.created_unix = now_unix();
            let existing = existing.clone();
            self.save(&index);
            return Ok(existing);
        }
        // Oldest first until the new file fits. `max_file_bytes <= quota_bytes` is validated.
        while index.iter().map(|entry| entry.size).sum::<u64>() + upload.size
            > self.limits.quota_bytes
            && !index.is_empty()
        {
            let oldest = (0..index.len())
                .min_by_key(|position| index[*position].created_unix)
                .expect("index is not empty");
            self.remove(&mut index, oldest);
        }
        let entry = Entry {
            hash,
            ext: ext.to_owned(),
            mime: mime.to_owned(),
            name,
            size: upload.size,
            created_unix: now_unix(),
        };
        fs::rename(&upload.path, self.path(&entry)).map_err(|_| MediaError::Io)?;
        index.push(entry.clone());
        self.save(&index);
        Ok(entry)
    }

    /// Copy an already opened file into the store (agent-produced files).
    pub fn ingest(&self, mut source: File, name: &str) -> Result<Entry, MediaError> {
        let mut upload = self.begin()?;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer).map_err(|_| MediaError::Io)?;
            if read == 0 {
                break;
            }
            upload.write(&buffer[..read])?;
        }
        self.finish(upload, name)
    }

    /// Look up `<hash>.<ext>`. Returns the entry and the seconds it has left.
    pub fn get(&self, file_name: &str) -> Option<(Entry, u64)> {
        let index = self.index.lock().unwrap_or_else(|lock| lock.into_inner());
        let entry = index
            .iter()
            .find(|entry| entry.file_name() == file_name)?
            .clone();
        let age = now_unix().saturating_sub(entry.created_unix);
        (age < self.limits.ttl_seconds).then(|| (entry, self.limits.ttl_seconds - age))
    }

    pub fn by_hash(&self, hash: &str) -> Option<Entry> {
        let index = self.index.lock().unwrap_or_else(|lock| lock.into_inner());
        let cutoff = now_unix().saturating_sub(self.limits.ttl_seconds);
        index
            .iter()
            .find(|entry| entry.hash == hash && entry.created_unix > cutoff)
            .cloned()
    }

    /// Map a stored file path, as written into a prompt, back to its entry.
    pub fn by_path(&self, path: &Path) -> Option<Entry> {
        (path.parent()? == self.dir)
            .then(|| path.file_name()?.to_str())
            .flatten()
            .and_then(|name| self.get(name))
            .map(|(entry, _)| entry)
    }
}

fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|character| !character.is_control() && !matches!(character, '"' | '<' | '>'))
        .take(120)
        .collect();
    if cleaned.trim_matches(['.', ' ']).is_empty() {
        "file".to_owned()
    } else {
        cleaned
    }
}

/// Allowlist by content signature. The upload's own name and Content-Type are ignored.
fn sniff(head: &[u8]) -> Option<(&'static str, &'static str)> {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    if at(0, b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if at(0, b"\xff\xd8\xff") {
        Some(("jpg", "image/jpeg"))
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        Some(("gif", "image/gif"))
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        Some(("webp", "image/webp"))
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        Some(("wav", "audio/wav"))
    } else if at(0, b"%PDF-") {
        Some(("pdf", "application/pdf"))
    } else if at(4, b"ftyp") {
        Some(("mp4", "video/mp4"))
    } else if at(0, b"\x1a\x45\xdf\xa3") {
        Some(("webm", "video/webm"))
    } else if at(0, b"ID3") {
        Some(("mp3", "audio/mpeg"))
    } else {
        None
    }
}
