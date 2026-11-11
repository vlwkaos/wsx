//! `wsx memory`: the contract's commands over `wsx_core::memory`. ref: docs/memory.md

use super::*;
use serde_json::{json, Value};
use std::io::Read;
use wsx_core::config::project::load_project_config;
use wsx_core::memory::manage::{self, Drift};
use wsx_core::memory::start;
use wsx_core::memory::{
    backend, known_scopes, validate_scope, Command, Error, NewRecord, Patch, Query, Record, Store,
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
            let query = Query {
                words: query,
                limit,
                include_inactive,
                kind,
            };
            show_hits(
                backend::search(&stores(&scopes)?, external().as_ref(), &query)?,
                json,
            )?;
        }
        MemoryCmd::Recall { question } => {
            show_hits(
                backend::recall(&stores(&scopes)?, external().as_ref(), &question)?,
                json,
            )?;
        }
        MemoryCmd::Add {
            kind,
            slug,
            text_file,
            source,
            title,
            verified_by,
            commit,
            description,
            keywords,
            paths,
            words,
            supersedes,
        } => {
            let store = one(&scopes)?;
            let text = read_text(&text_file)?;
            let added = manage::add(
                &store,
                NewRecord {
                    kind,
                    slug,
                    text,
                    title,
                    source,
                    verified_by,
                    commit,
                    description,
                    keywords,
                    paths,
                    words,
                },
                supersedes.as_deref(),
            )?;
            notice_drift(&added.drift);
            let verb = if added.written {
                "added"
            } else {
                "no change, same as"
            };
            // ^ The answer stays a record; the close ones ride along as one more key.
            if json {
                return match added.similar.is_empty() {
                    true => print(&added.record),
                    false => print(&WithSimilar {
                        record: &added.record,
                        similar: &added.similar,
                    }),
                };
            }
            show(&added.record, false, verb)?;
            for hit in &added.similar {
                println!(
                    "close to {} [{:.2}]: {} (replace it with `supersede {} {}`, or add with --supersedes)",
                    hit.slug,
                    hit.score,
                    hit.excerpt.replace('\n', " "),
                    hit.slug,
                    added.record.slug
                );
            }
        }
        MemoryCmd::Update {
            slug,
            text_file,
            source,
            verified_by,
            commit,
            description,
            keywords,
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
            let (record, drift) = manage::update(
                &store,
                Patch {
                    slug,
                    text,
                    source,
                    verified_by,
                    commit,
                    description,
                    keywords,
                    fail: lesson,
                },
            )?;
            notice_drift(&drift);
            show(&record, json, "updated")?;
        }
        MemoryCmd::Capture {
            task,
            memo,
            commit,
            paths,
        } => {
            let added = manage::capture(&one(&scopes)?, task, &memo, commit.as_deref(), &paths)?;
            notice_drift(&added.drift);
            show(
                &added.record,
                json,
                if added.written {
                    "captured"
                } else {
                    "no change, same as"
                },
            )?;
        }
        MemoryCmd::Start { files, role } => {
            let store = one(&scopes)?;
            let areas = match store.project_ids() {
                Some(_) => load_project_config(store.root()).memory_areas,
                None => Vec::new(),
            };
            let text = start::block(
                &store,
                &start::Start {
                    files: &files,
                    areas: &areas,
                    role: role.as_deref(),
                },
                start::BUDGET_BYTES,
            );
            match json {
                true => print(&json!({ "scope": store.scope(), "start": text }))?,
                false => println!("{text}"),
            }
        }
        MemoryCmd::Supersede { old, new, source } => {
            let (record, drift) = manage::supersede(&one(&scopes)?, &old, &new, &source)?;
            notice_drift(&drift);
            show(&record, json, "superseded")?;
        }
        MemoryCmd::Get { slug } => {
            show(&one(&scopes)?.get(&slug)?, json, "record")?;
        }
        MemoryCmd::Import => {
            let store = one(&scopes)?;
            let (imported, drift) = manage::import(&store)?;
            notice_drift(&drift);
            if json {
                return print(&json!({ "scope": store.scope(), "imported": imported }));
            }
            println!(
                "{}: imported {} rules, {} replaced rules, {} facts, {} episodes",
                store.scope(),
                imported.rules,
                imported.archived,
                imported.facts,
                imported.episodes
            );
        }
        MemoryCmd::Reindex => {
            let store = one(&scopes)?;
            backend::reindex(&store, external().as_ref())?;
            let out = json!({ "scope": store.scope(), "ok": true });
            if json {
                return print(&out);
            }
            println!("{}: ok", store.scope());
        }
        MemoryCmd::Status => {
            let store = one(&scopes)?;
            let [active, superseded, failed] = store.counts()?;
            let (name, flags, error) = backend::describe(&store, external().as_ref());
            let mut out = json!({
                "scope": store.scope(),
                "store": store.dir(),
                "backend": name,
                "search": { "network": flags.network, "model": flags.model },
                "limits": { "text_bytes": store.text_bytes() },
                "counts": { "active": active, "superseded": superseded, "failed": failed },
            });
            if let Some(error) = &error {
                out["backend_error"] = json!(error);
            }
            if store.project_ids().is_some() {
                out["drift"] = json!(manage::drift(&store)?);
            }
            if json {
                return print(&out);
            }
            println!(
                "{}: {}\nbackend {name} (network {}, model {})\ntext up to {} bytes\n{active} active, {superseded} superseded, {failed} failed",
                store.scope(),
                store.dir().display(),
                flags.network,
                flags.model,
                store.text_bytes()
            );
            if let Some(error) = error {
                println!("backend unavailable: {error}; the built-in answers");
            }
        }
        MemoryCmd::List => {
            let ids: Vec<String> = projects()?.into_iter().map(|(id, ..)| id).collect();
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

/// A record and the active records of its kind that it is close to.
#[derive(serde::Serialize)]
struct WithSimilar<'a> {
    #[serde(flatten)]
    record: &'a Record,
    similar: &'a [backend::Hit],
}

/// A view edited by hand was kept and written again from the records (stderr).
fn notice_drift(drift: &[Drift]) {
    for d in drift {
        eprintln!("wsx: memory: {}", d.notice(true));
    }
}

fn show(record: &Record, json: bool, verb: &str) -> MemResult<()> {
    if json {
        return print(record);
    }
    println!(
        "{verb} {} [{}, {}]",
        record.slug,
        record.kind.as_str(),
        record.status.as_str()
    );
    Ok(())
}

/// The search backend of the global config, when one is set. A config that cannot be read is
/// said on stderr, and the built-in answers.
fn external() -> Option<Command> {
    match load_config() {
        Ok(config) => Command::new(config.memory.backend),
        Err(error) => {
            eprintln!("wsx: cannot read the config ({error:#}); the built-in index answers");
            None
        }
    }
}

/// Print the hits (a notice for each failed backend goes to stderr).
fn show_hits(answer: backend::Answer, json: bool) -> MemResult<()> {
    for notice in &answer.notices {
        eprintln!("wsx: {notice}");
    }
    if json {
        return print(&answer.hits);
    }
    for hit in &answer.hits {
        println!(
            "{:.2} {}/{} [{}, {}{}] {}",
            hit.score,
            hit.scope,
            hit.slug,
            hit.kind,
            hit.status,
            if hit.extra.get("stale").is_some() {
                ", stale"
            } else {
                ""
            },
            hit.excerpt.replace('\n', " ")
        );
    }
    Ok(())
}

/// The registered projects as `(task ID, name, path)`.
fn projects() -> MemResult<Vec<(String, String, PathBuf)>> {
    let config =
        load_config().map_err(|e| Error::Store(format!("cannot read the config: {e:#}")))?;
    Ok(config
        .projects
        .into_iter()
        .map(|p| {
            (
                wsx_core::task::store::project_id(&p.name, &p.path),
                p.name,
                p.path,
            )
        })
        .collect())
}

fn one(scopes: &[String]) -> MemResult<Store> {
    let mut found = stores(scopes)?;
    match found.len() {
        1 => Ok(found.remove(0)),
        _ => Err(Error::Usage("this command takes one --scope".into())),
    }
}

/// The stores of `scoped`, cut down for an agent (the broker sets `policy::RULES_ENV` and
/// `FROM_ENV`): no rules unless it may write one, and the transcripts only of its own project.
fn stores(scopes: &[String]) -> MemResult<Vec<Store>> {
    let rules = std::env::var(wsx_core::task::policy::RULES_ENV).as_deref() != Ok("deny");
    let own = std::env::var(crate::task::broker::FROM_ENV).ok();
    Ok(scoped(scopes)?
        .into_iter()
        .map(|store| if rules { store } else { store.without_rules() })
        .map(|store| match (&own, store.project_ids()) {
            (Some(own), Some((id, _))) if own != id => store.without_transcripts(),
            _ => store,
        })
        .collect())
}

/// The store of each `--scope`, or of the project of the current folder when none is given.
/// A name that is no registered project is an unbound scope; its relative paths start at the
/// current folder.
fn scoped(scopes: &[String]) -> MemResult<Vec<Store>> {
    let cwd = std::env::current_dir()?;
    // ^ `default` needs no config, so it works where the config cannot be read.
    let projects = if scopes.iter().all(|s| s == "default") && !scopes.is_empty() {
        Vec::new()
    } else {
        projects()?
    };
    if scopes.is_empty() {
        let (id, name, path) = projects
            .iter()
            .filter(|(.., path)| cwd.starts_with(path) || real_path_inside(&cwd, path))
            .max_by_key(|(.., path)| path.as_os_str().len())
            .ok_or_else(|| {
                Error::Usage("the current folder is in no project: give --scope".into())
            })?;
        return Ok(vec![Store::project(id, name, path)?]);
    }
    scopes
        .iter()
        .map(|scope| {
            validate_scope(scope)?;
            match projects.iter().find(|(id, ..)| id == scope) {
                Some((id, name, path)) => Store::project(id, name, path),
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
