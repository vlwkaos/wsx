//! Per-site, content-addressed media store. See docs/web-client.md "Media".
//! ^ A URL names the SHA-256 of the bytes, so a browser may cache it as immutable.
//! An entry lives from its first upload until the TTL or quota removes it.
use crate::compact::{self, Compact};
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
    /// The compact copy an agent reads first (`compact.rs`); freed with the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<Compact>,
    /// The copy is being made now (in memory only: a restart loses the job and its files).
    #[serde(skip)]
    pub preparing: bool,
}

impl Entry {
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.hash, self.ext)
    }
    /// Bytes on disk: the original and its compact copy, as the quota counts them.
    fn stored_bytes(&self) -> u64 {
        self.size + self.compact.as_ref().map_or(0, |compact| compact.size)
    }
    /// Every file the entry keeps in the media directory: the original, then its compact
    /// copy's.
    fn files(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(self.file_name()).chain(
            self.compact
                .iter()
                .flat_map(|copy| copy.files.iter().cloned()),
        )
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
        // A copy with a missing file is dropped whole; the original stays.
        for entry in index.iter_mut() {
            let broken = entry
                .compact
                .as_ref()
                .is_some_and(|copy| copy.files.iter().any(|name| !dir.join(name).exists()));
            if broken {
                for name in entry.compact.take().iter().flat_map(|copy| &copy.files) {
                    compact::remove(&dir, name);
                }
            }
        }
        for item in fs::read_dir(&dir)?.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let known = name == "index.json"
                || index
                    .iter()
                    .any(|entry| entry.files().any(|file| file == name));
            if !known {
                compact::remove(&dir, &name);
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
        for name in entry.files() {
            compact::remove(&self.dir, &name);
        }
    }

    /// Remove expired entries. Called by the server's sweeper and before each write.
    pub fn sweep(&self) -> usize {
        let mut index = crate::locked(&self.index);
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

    pub fn finish(&self, upload: Upload, name: &str) -> Result<Entry, MediaError> {
        self.finish_if(upload, name, |_| true)
    }

    /// `finish` for a file whose sniffed mime `accept` takes; another type is refused and the
    /// upload's partial file goes with it.
    fn finish_if(
        &self,
        mut upload: Upload,
        name: &str,
        accept: impl Fn(&str) -> bool,
    ) -> Result<Entry, MediaError> {
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
        if !accept(mime) {
            return Err(MediaError::UnsupportedType);
        }
        let hash = hex(&upload.hasher.clone().finalize());
        self.sweep();
        let mut index = crate::locked(&self.index);
        // The same bytes again: keep the entry and restart its TTL, so a file that
        // was just attached is not removed moments later.
        if let Some(existing) = index.iter_mut().find(|entry| entry.hash == hash) {
            existing.created_unix = now_unix();
            let existing = existing.clone();
            self.save(&index);
            return Ok(existing);
        }
        self.make_room(&mut index, upload.size, "");
        let entry = Entry {
            hash,
            ext: ext.to_owned(),
            mime: mime.to_owned(),
            name,
            size: upload.size,
            created_unix: now_unix(),
            compact: None,
            preparing: false,
        };
        fs::rename(&upload.path, self.path(&entry)).map_err(|_| MediaError::Io)?;
        index.push(entry.clone());
        self.save(&index);
        Ok(entry)
    }

    /// Oldest first (never `keep`) until `extra` more bytes fit. `max_file_bytes <=
    /// quota_bytes` is validated.
    fn make_room(&self, index: &mut Vec<Entry>, extra: u64, keep: &str) {
        while index.iter().map(Entry::stored_bytes).sum::<u64>() + extra > self.limits.quota_bytes {
            let oldest = (0..index.len())
                .filter(|position| index[*position].hash != keep)
                .min_by_key(|position| index[*position].created_unix);
            match oldest {
                Some(oldest) => self.remove(index, oldest),
                None => break,
            }
        }
    }

    /// Claim the making of an entry's compact copy: the entry when a handler exists, it has
    /// no copy and no one makes one. The claimer then calls `prepare`, which ends it.
    pub fn claim_prepare(&self, hash: &str) -> Option<Entry> {
        let mut index = crate::locked(&self.index);
        let row = index.iter_mut().find(|row| row.hash == hash)?;
        if row.compact.is_some() || row.preparing || !compact::handles(&row.ext) {
            return None;
        }
        row.preparing = true;
        Some(row.clone())
    }

    /// Make the entry's compact copy outside the index lock (a tool can take minutes), then
    /// record it and end the claim, with or without a copy. An entry removed meanwhile takes
    /// its new copy with it.
    pub fn prepare(&self, hash: &str, ext: &str) -> Option<Entry> {
        self.attach_copy(hash, || compact::make(&self.dir, hash, ext))
    }

    /// `prepare` with the copy's maker given. The claim ends also when `make` panics: a
    /// guard clears `preparing`, or the page would ask `media_state` for ever.
    fn attach_copy(&self, hash: &str, make: impl FnOnce() -> Option<Compact>) -> Option<Entry> {
        struct EndClaim<'a>(&'a Media, &'a str);
        impl Drop for EndClaim<'_> {
            fn drop(&mut self) {
                let mut index = crate::locked(&self.0.index);
                if let Some(row) = index.iter_mut().find(|row| row.hash == self.1) {
                    row.preparing = false;
                }
            }
        }
        let _claim = EndClaim(self, hash);
        let copy = make();
        let mut index = crate::locked(&self.index);
        let Some(row) = index.iter_mut().find(|row| row.hash == hash) else {
            copy.iter()
                .flat_map(|copy| &copy.files)
                .for_each(|name| compact::remove(&self.dir, name));
            return None;
        };
        row.preparing = false;
        row.compact = copy;
        let entry = row.clone();
        self.make_room(&mut index, 0, hash);
        self.save(&index);
        Some(entry)
    }

    /// The prompt line of an attachment: `path (name)`; with a compact copy that path
    /// first and the original after it.
    pub fn prompt_line(&self, entry: &Entry) -> String {
        let original = self.path(entry);
        let Some((copy, first)) =
            (entry.compact.as_ref()).and_then(|copy| copy.files.first().map(|first| (copy, first)))
        else {
            return format!("{} ({})", original.display(), entry.name);
        };
        format!(
            "{} ({}: {}) original: {} (open it only for {})",
            self.dir.join(first).display(),
            entry.name,
            copy.summary,
            original.display(),
            copy.original_for
        )
    }

    /// Copy an already opened file into the store (agent-produced files, files a message
    /// shows); `accept` takes the sniffed mime.
    pub fn ingest(
        &self,
        mut source: File,
        name: &str,
        accept: impl Fn(&str) -> bool,
    ) -> Result<Entry, MediaError> {
        let mut upload = self.begin()?;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer).map_err(|_| MediaError::Io)?;
            if read == 0 {
                break;
            }
            upload.write(&buffer[..read])?;
        }
        self.finish_if(upload, name, accept)
    }

    /// Look up `<hash>.<ext>`. Returns the entry and the seconds it has left.
    pub fn get(&self, file_name: &str) -> Option<(Entry, u64)> {
        let index = crate::locked(&self.index);
        let entry = index
            .iter()
            .find(|entry| entry.file_name() == file_name)?
            .clone();
        let age = now_unix().saturating_sub(entry.created_unix);
        (age < self.limits.ttl_seconds).then(|| (entry, self.limits.ttl_seconds - age))
    }

    pub fn by_hash(&self, hash: &str) -> Option<Entry> {
        let index = crate::locked(&self.index);
        let cutoff = now_unix().saturating_sub(self.limits.ttl_seconds);
        index
            .iter()
            .find(|entry| entry.hash == hash && entry.created_unix > cutoff)
            .cloned()
    }

    /// The entry a prompt line names (`prompt_line`): the stored path of the original,
    /// after `original:` when the line also names a compact copy.
    pub fn by_line(&self, line: &str) -> Option<Entry> {
        let rest = line
            .split_once(") original: ")
            .map_or(line, |(_, rest)| rest);
        let name = rest.strip_prefix(self.dir.to_str()?)?.strip_prefix('/')?;
        self.get(name.split_once(" (").map_or(name, |(name, _)| name))
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
pub(crate) fn sniff(head: &[u8]) -> Option<(&'static str, &'static str)> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn store() -> (Media, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "wsx-media-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let media = Media::open(&root, MediaLimits::default()).unwrap();
        (media, root)
    }

    fn pdf(media: &Media) -> Entry {
        let mut upload = media.begin().unwrap();
        upload.write(b"%PDF-1.4 a real enough file").unwrap();
        media.finish(upload, "doc.pdf").unwrap()
    }

    /// A copy that fails, one that panics and one with no files all end the claim, so the
    /// chip stops polling; the prompt line of the empty copy names the original.
    #[test]
    fn a_failed_or_panicking_copy_clears_preparing() {
        let (media, root) = store();
        let entry = pdf(&media);
        let preparing = || media.by_hash(&entry.hash).unwrap().preparing;

        assert!(media.claim_prepare(&entry.hash).is_some());
        assert!(preparing());
        assert!(media.attach_copy(&entry.hash, || None).is_some());
        assert!(!preparing());

        assert!(media.claim_prepare(&entry.hash).is_some());
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            media.attach_copy(&entry.hash, || panic!("whisper died"))
        }));
        assert!(panicked.is_err());
        assert!(!preparing());

        assert!(media.claim_prepare(&entry.hash).is_some());
        let empty = Compact {
            files: Vec::new(),
            summary: "none".to_owned(),
            original_for: "all".to_owned(),
            size: 0,
        };
        let entry = media.attach_copy(&entry.hash, || Some(empty)).unwrap();
        assert!(!preparing());
        assert_eq!(
            media.prompt_line(&entry),
            format!("{} (doc.pdf)", media.path(&entry).display())
        );
        let _ = fs::remove_dir_all(root);
    }
}
