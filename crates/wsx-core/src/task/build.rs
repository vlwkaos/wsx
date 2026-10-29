//! A working copy's cargo build folder: what the clone leaves out and what goes stale.
//! ref: docs/tasks.md "Build output"

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The sources to clone into each destination: every top-level entry of `main` but `.git`,
/// with `target/<profile>/incremental` left out. A copy builds with `CARGO_INCREMENTAL=0`
/// and never reads it, and once main's next build replaces it the clone's blocks are the
/// copy's own (5.4 GB for wsx).
pub(super) fn clone_groups(main: &Path, copy: &Path) -> Result<Vec<(Vec<PathBuf>, PathBuf)>> {
    let mut top = entries(main, |name| name != ".git")?;
    let target = main.join("target");
    if !fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_dir()) {
        return Ok(vec![(top, copy.to_path_buf())]);
    }
    top.retain(|entry| entry != &target);
    let mut groups = vec![(top, copy.to_path_buf())];
    let mut plain = Vec::new();
    for entry in entries(&target, |_| true)? {
        if entry.join("incremental").is_dir() {
            let destination = copy
                .join("target")
                .join(entry.file_name().unwrap_or_default());
            groups.push((entries(&entry, |name| name != "incremental")?, destination));
        } else {
            plain.push(entry);
        }
    }
    groups.push((plain, copy.join("target")));
    groups.retain(|(sources, _)| !sources.is_empty());
    Ok(groups)
}

fn entries(dir: &Path, keep: impl Fn(&str) -> bool) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("cannot list {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| keep(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    Ok(entries)
}

/// Remove the build output in `target/*/deps` that no build of this copy uses, and return
/// the bytes it took. Best effort: a file that cannot be read or removed stays.
///
/// 1. A unit (`<name>-<hash>`: its rlib, rmeta, binary, `.d` and objects) older than `since`,
///    the copy's birth, when a unit of the same name was built since: the clone's units of
///    main's other settings (main builds incrementally, so its workspace crates and test
///    binaries have other hashes). A wrong guess only costs a rebuild: cargo rebuilds a unit
///    whose output is gone.
/// 2. In a unit, the objects of all but the newest rustc session. On macOS the dev profile's
///    `split-debuginfo=unpacked` keeps a binary's `.rcgu.o` files beside it for its debug
///    info, and an incremental build names them by session and never removes the old ones
///    (about 0.2 GB per wsx rebuild). The binary refers only to its last session's objects.
pub fn prune(copy: &Path, since: SystemTime) -> u64 {
    let Ok(profiles) = fs::read_dir(copy.join("target")) else {
        return 0;
    };
    profiles
        .filter_map(|entry| entry.ok())
        .map(|entry| prune_deps(&entry.path().join("deps"), since))
        .sum()
}

struct File {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
    /// The rustc session of an incremental object (`<unit>.<cgu>.<session>.rcgu.o`); empty
    /// for any other file.
    session: String,
}

fn prune_deps(deps: &Path, since: SystemTime) -> u64 {
    let Ok(read) = fs::read_dir(deps) else {
        return 0;
    };
    // ^ name -> hash -> files
    let mut units: BTreeMap<String, BTreeMap<String, Vec<File>>> = BTreeMap::new();
    for entry in read.filter_map(|entry| entry.ok()) {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some((name, hash, session)) = unit_of(&file_name) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        units
            .entry(name)
            .or_default()
            .entry(hash)
            .or_default()
            .push(File {
                path: entry.path(),
                bytes: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                session,
            });
    }
    let newest = |files: &[File]| files.iter().map(|file| file.modified).max();
    let mut stale = Vec::new();
    for hashes in units.into_values() {
        let rebuilt = hashes
            .values()
            .any(|files| newest(files).is_some_and(|time| time >= since));
        for files in hashes.into_values() {
            if rebuilt && newest(&files).is_some_and(|time| time < since) {
                stale.extend(files);
                continue;
            }
            // ^ A session reuses the unchanged objects of the one before with their times, so
            // two sessions can end at the same time: both stay.
            let mut ends: BTreeMap<&str, SystemTime> = BTreeMap::new();
            for file in files.iter().filter(|file| !file.session.is_empty()) {
                let end = ends.entry(&file.session).or_insert(file.modified);
                *end = (*end).max(file.modified);
            }
            let last = ends.values().max().copied();
            let old: Vec<String> = ends
                .iter()
                .filter(|(_, end)| Some(**end) != last)
                .map(|(session, _)| session.to_string())
                .collect();
            stale.extend(files.into_iter().filter(|file| old.contains(&file.session)));
        }
    }
    stale
        .into_iter()
        .filter(|file| fs::remove_file(&file.path).is_ok())
        .map(|file| file.bytes)
        .sum()
}

/// `(name, hash, session)` of a file in `deps`: `lib<name>-<hash>.rlib`, `<name>-<hash>`,
/// `<name>-<hash>.d`, `<name>-<hash>.<cgu>.<session>.rcgu.o` (incremental) or
/// `<name>-<hash>.<crate>.<cgu>-cgu.<n>.rcgu.o`.
fn unit_of(file_name: &str) -> Option<(String, String, String)> {
    let (stem, rest) = file_name.split_once('.').unwrap_or((file_name, ""));
    let (name, hash) = stem.rsplit_once('-')?;
    if hash.len() != 16 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let name = match rest {
        "rlib" | "rmeta" | "dylib" | "so" | "a" => name.strip_prefix("lib").unwrap_or(name),
        _ => name,
    };
    let session = rest
        .strip_suffix(".rcgu.o")
        .and_then(|object| object.split_once('.'))
        .filter(|(_, session)| !session.contains('.') && !session.contains("-cgu"))
        .map(|(_, session)| session.to_string())
        .unwrap_or_default();
    Some((name.to_string(), hash.to_string(), session))
}
