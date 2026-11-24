//! Which projects use a dependency, for mail between projects (`wsx task say --uses`): the
//! lockfiles and manifests at a project's root that name it as a whole word. A text match, not a
//! resolver: a name that only appears in a comment counts too. ref: docs/tasks.md "Mail"
use std::path::Path;

/// The files read, at the project's root.
pub const FILES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "package.json",
    "pnpm-lock.yaml",
    "package-lock.json",
    "yarn.lock",
    "bun.lock",
    "go.mod",
    "go.sum",
    "requirements.txt",
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "Gemfile.lock",
    "Podfile.lock",
    "Package.resolved",
];

/// A file larger than this is read only up to it.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The first of `FILES` at `root` that names `dep` as a whole word (no letter, digit, `_`, `-` or
/// `.` on either side; `@` and `/` may follow, as in `name@1.0` and `name/sub`), or `None`.
pub fn uses(root: &Path, dep: &str) -> Option<&'static str> {
    FILES.iter().copied().find(|file| {
        let Ok(handle) = std::fs::File::open(root.join(file)) else {
            return false;
        };
        // ^ Bytes, not a string: a file cut at the cap or not UTF-8 still reads.
        let mut bytes = Vec::new();
        use std::io::Read;
        if handle.take(MAX_BYTES).read_to_end(&mut bytes).is_err() {
            return false;
        }
        names(&String::from_utf8_lossy(&bytes), dep)
    })
}

/// A dependency name `say --uses` takes: 1 to 214 characters, a letter, digit or `@` first,
/// then letters, digits and `@ / . _ -` (npm scopes, Go paths, crates, Python packages).
pub fn checked_dep(dep: &str) -> anyhow::Result<&str> {
    let ok = !dep.is_empty()
        && dep.len() <= 214
        && dep.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '@')
        && dep.chars().any(|c| c.is_ascii_alphanumeric())
        && dep
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@/._-".contains(c));
    anyhow::ensure!(
        ok,
        "{dep:?} is not a dependency name: 1 to 214 of letters, digits and @ / . _ -; nothing was sent"
    );
    Ok(dep)
}

fn names(text: &str, dep: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.');
    text.match_indices(dep).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + dep.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}
