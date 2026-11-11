//! `wsx memory`: the contract's commands over `wsx_core::memory`. ref: docs/memory.md

use super::*;
use serde_json::{json, Value};
use std::io::Read;
use wsx_core::memory::{
    known_scopes, validate_scope, Error, Hit, NewRecord, Patch, Record, SearchOptions, Store,
};

type MemResult<T> = std::result::Result<T, Error>;

/// Run one command; a failure prints `wsx: <message>` and exits with the contract's code.
pub(super) fn cmd_memory(command: MemoryCmd, scopes: Vec<String>, json: bool) -> Result<()> {
    if let Err(error) = run_memory(command, scopes, json) {
        eprintln!("wsx: {error}");
        std::process::exit(error.code());
    }
    Ok(())
}

fn run_memory(command: MemoryCmd, scopes: Vec<String>, json: bool) -> MemResult<()> {
    match command {
        MemoryCmd::Search {
            query,
            limit,
            include_inactive,
            kind,
        } => {
            let options = SearchOptions {
                limit,
                include_inactive,
                kind,
            };
            let mut hits: Vec<Hit> = Vec::new();
            for store in stores(&scopes)? {
                hits.extend(store.search(&query, &options)?);
            }
            // ^ Merged by score; each store's hits already come newer first on a tie.
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            hits.truncate(limit);
            if json {
                return print(&hits);
            }
            for hit in &hits {
                println!(
                    "{:.2} {}/{} [{}, {}] {}",
                    hit.score,
                    hit.scope,
                    hit.slug,
                    hit.kind.as_str(),
                    hit.status.as_str(),
                    hit.excerpt.replace('\n', " ")
                );
            }
        }
        MemoryCmd::Add {
            kind,
            slug,
            text_file,
            source,
            title,
            verified_by,
            commit,
        } => {
            let store = one(&scopes)?;
            let text = read_text(&text_file)?;
            let (record, written) = store.add(NewRecord {
                kind,
                slug,
                text,
                title,
                source,
                verified_by,
                commit,
            })?;
            show(
                &record,
                json,
                if written {
                    "added"
                } else {
                    "no change, same as"
                },
            )?;
        }
        MemoryCmd::Update {
            slug,
            text_file,
            source,
            verified_by,
            commit,
            status,
            lesson,
        } => {
            let store = one(&scopes)?;
            let text = text_file.as_deref().map(read_text).transpose()?;
            if status.is_none() && lesson.is_some() {
                return Err(Error::Usage("--lesson goes with --status failed".into()));
            }
            if status.is_some() && lesson.is_none() {
                return Err(Error::Usage("--status failed needs a --lesson".into()));
            }
            let record = store.update(Patch {
                slug,
                text,
                source,
                verified_by,
                commit,
                fail: lesson,
            })?;
            show(&record, json, "updated")?;
        }
        MemoryCmd::Supersede { old, new, source } => {
            let record = one(&scopes)?.supersede(&old, &new, &source)?;
            show(&record, json, "superseded")?;
        }
        MemoryCmd::Reindex => {
            let store = one(&scopes)?;
            // ^ The built-in search reads the log itself; this checks that the log folds.
            store.counts()?;
            let out = json!({ "scope": store.scope(), "ok": true });
            if json {
                return print(&out);
            }
            println!("{}: ok", store.scope());
        }
        MemoryCmd::Status => {
            let store = one(&scopes)?;
            let [active, superseded, failed] = store.counts()?;
            let out = json!({
                "scope": store.scope(),
                "store": store.dir(),
                "backend": "builtin",
                "search": { "network": false, "model": false },
                "counts": { "active": active, "superseded": superseded, "failed": failed },
            });
            if json {
                return print(&out);
            }
            println!(
                "{}: {}\nbackend builtin (no network, no model)\n{active} active, {superseded} superseded, {failed} failed",
                store.scope(),
                store.dir().display()
            );
        }
        MemoryCmd::List => {
            let ids: Vec<String> = projects()?.into_iter().map(|(id, _)| id).collect();
            let rows: Vec<Value> = known_scopes(&ids)
                .into_iter()
                .map(|(scope, store)| json!({ "scope": scope, "store": store }))
                .collect();
            if json {
                return print(&rows);
            }
            for row in &rows {
                println!(
                    "{} {}",
                    row["scope"].as_str().unwrap_or(""),
                    row["store"].as_str().unwrap_or("")
                );
            }
        }
    }
    Ok(())
}

fn print(value: &impl serde::Serialize) -> MemResult<()> {
    println!(
        "{}",
        serde_json::to_string(value).map_err(|e| Error::Store(e.to_string()))?
    );
    Ok(())
}

fn show(record: &Record, json: bool, verb: &str) -> MemResult<()> {
    if json {
        return print(record);
    }
    println!(
        "{verb} {} [{:?}, {:?}]",
        record.slug, record.kind, record.status
    );
    Ok(())
}

/// The registered projects as `(task ID, path)`.
fn projects() -> MemResult<Vec<(String, PathBuf)>> {
    let config =
        load_config().map_err(|e| Error::Store(format!("cannot read the config: {e:#}")))?;
    Ok(config
        .projects
        .into_iter()
        .map(|p| (wsx_core::task::store::project_id(&p.name, &p.path), p.path))
        .collect())
}

fn one(scopes: &[String]) -> MemResult<Store> {
    let mut found = stores(scopes)?;
    match found.len() {
        1 => Ok(found.remove(0)),
        _ => Err(Error::Usage("this command takes one --scope".into())),
    }
}

/// The store of each `--scope`, or of the project of the current folder when none is given.
/// A name that is no registered project is an unbound scope; its relative paths start at the
/// current folder.
fn stores(scopes: &[String]) -> MemResult<Vec<Store>> {
    let cwd = std::env::current_dir()?;
    // ^ `default` needs no config, so it works where the config cannot be read.
    let projects = if scopes.iter().all(|s| s == "default") && !scopes.is_empty() {
        Vec::new()
    } else {
        projects()?
    };
    if scopes.is_empty() {
        let (id, path) = projects
            .iter()
            .filter(|(_, path)| cwd.starts_with(path) || real_path_inside(&cwd, path))
            .max_by_key(|(_, path)| path.as_os_str().len())
            .ok_or_else(|| {
                Error::Usage("the current folder is in no project: give --scope".into())
            })?;
        return Ok(vec![Store::project(id, path)?]);
    }
    scopes
        .iter()
        .map(|scope| {
            validate_scope(scope)?;
            match projects.iter().find(|(id, _)| id == scope) {
                Some((id, path)) => Store::project(id, path),
                None => Store::named(scope, &cwd),
            }
        })
        .collect()
}

/// A project reached through a link: compare the real paths.
fn real_path_inside(cwd: &Path, project: &Path) -> bool {
    std::fs::canonicalize(project).is_ok_and(|real| cwd.starts_with(real))
}

/// The text of `--text-file`: a file, or stdin for `-`. At most one byte over the cap is read.
fn read_text(source: &str) -> MemResult<String> {
    const CAP: u64 = 8192;
    let mut text = String::new();
    let read = if source == "-" {
        std::io::stdin().take(CAP).read_to_string(&mut text)
    } else {
        std::fs::File::open(source).and_then(|file| file.take(CAP).read_to_string(&mut text))
    };
    read.map_err(|e| Error::Usage(format!("cannot read the text from {source}: {e}")))?;
    Ok(text)
}
