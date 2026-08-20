//! Agent-friendly resource CLI.
//! ref: README.md#cli

use anyhow::{bail, Context, Result};
use asched_core::routine::ipc::{Action, Request, Response, RoutineView};
use asched_core::routine::{FireOutcome, Routine, RoutineFire, Trigger, STARTUP_FD_ENV};
use asched_core::{Project, RegistryStore};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};

use crate::service::{client, daemon_command, registry, resolve_project, send, terminal_safe};

#[derive(Debug, Parser)]
#[command(name = "asched", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Register and inspect working-directory projects
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage scheduled routines
    Routine {
        #[command(subcommand)]
        command: RoutineCommand,
    },
    /// Manage the local scheduler daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Import routines from an earlier scheduler
    Migrate {
        #[command(subcommand)]
        command: MigrationCommand,
    },
    /// Internal daemon entrypoint
    #[command(hide = true, name = "daemon-serve")]
    DaemonServe,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// List registered projects
    List {
        #[arg(long)]
        json: bool,
    },
    /// Register a name and command working directory
    Add {
        name: String,
        working_dir: PathBuf,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Unregister a project without deleting its routine data
    Remove {
        name: String,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TriggerSelection {
    Cron,
    Event,
}

#[derive(Debug, Subcommand)]
pub enum RoutineCommand {
    /// List routines across selected projects
    List {
        /// Exact project name. Repeat to select multiple projects.
        #[arg(short, long)]
        project: Vec<String>,
        /// Case-insensitive substring matched against project name or working directory.
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, value_enum)]
        trigger: Option<TriggerSelection>,
        #[arg(long)]
        event_kind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        json: bool,
    },
    Add {
        name: String,
        #[arg(long, required_unless_present = "event", conflicts_with = "event")]
        cron: Option<String>,
        #[arg(long, required_unless_present = "cron", conflicts_with = "cron")]
        event: Option<String>,
        /// One direct argv item. Repeat in execution order.
        #[arg(long = "arg", allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
        #[arg(long, default_value = "")]
        prompt: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        disabled: bool,
        #[arg(long)]
        json: bool,
    },
    Edit {
        name: String,
        #[arg(long)]
        new_name: Option<String>,
        #[arg(long, required_unless_present = "event", conflicts_with = "event")]
        cron: Option<String>,
        #[arg(long, required_unless_present = "cron", conflicts_with = "cron")]
        event: Option<String>,
        #[arg(long = "arg", allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
        #[arg(long, default_value = "")]
        prompt: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    Delete {
        name: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    Enable {
        name: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    Disable {
        name: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    Run {
        name: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// Fire a provider-neutral event for one project
    Fire {
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        event_id: String,
        #[arg(
            long,
            required_unless_present = "payload_file",
            conflicts_with = "payload_file"
        )]
        payload: Option<String>,
        #[arg(long, required_unless_present = "payload", conflicts_with = "payload")]
        payload_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Cancel {
        name: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        json: bool,
    },
    Logs {
        name: String,
        #[arg(short, long)]
        project: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    Start,
    Status {
        #[arg(long)]
        json: bool,
    },
    Stop {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum MigrationCommand {
    /// Import wsx projects and routine definitions
    Wsx {
        /// Inspect the import without writing.
        #[arg(long)]
        dry_run: bool,
        /// Preserve enabled flags. By default imports are disabled to prevent duplicate runs.
        #[arg(long)]
        keep_enabled: bool,
        #[arg(long)]
        source_root: Option<PathBuf>,
        #[arg(long)]
        source_config: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

// ^ [[CLI and TUI Client Boundaries]]
pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Project { command } => run_project(command),
        Command::Routine { command } => run_routine(command),
        Command::Daemon { command } => run_daemon(command),
        Command::Migrate { command } => run_migration(command),
        Command::DaemonServe => serve_daemon(),
    }
}

fn run_migration(command: MigrationCommand) -> Result<()> {
    match command {
        MigrationCommand::Wsx {
            dry_run,
            keep_enabled,
            source_root,
            source_config,
            json,
        } => {
            let (default_root, default_config) = asched_core::migration::default_wsx_paths()?;
            let plan = asched_core::migration::plan_wsx_import(
                source_root.as_deref().unwrap_or(&default_root),
                source_config.as_deref().unwrap_or(&default_config),
            )?;
            if dry_run {
                if json {
                    println!("{}", serde_json::to_string_pretty(&plan)?);
                } else {
                    println!(
                        "{} project(s), {} routine(s); no changes made",
                        plan.projects.len(),
                        plan.projects
                            .iter()
                            .map(|project| project.routine_count)
                            .sum::<usize>()
                    );
                    for item in plan.projects {
                        println!(
                            "{}\t{}\t{} routine(s)",
                            terminal_safe(&item.project.name),
                            terminal_safe(&item.project.working_dir.to_string_lossy()),
                            item.routine_count
                        );
                    }
                }
                return Ok(());
            }
            let result =
                asched_core::migration::apply_wsx_import(&plan, &registry()?, keep_enabled)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "registered {} project(s), imported {} routine(s) from {} file(s)",
                    result.projects_registered,
                    result.routines_imported,
                    result.routine_files_imported
                );
                if !keep_enabled && result.routines_imported > 0 {
                    println!("imported routines are disabled; stop the wsx daemon before enabling");
                }
            }
            Ok(())
        }
    }
}

fn run_project(command: ProjectCommand) -> Result<()> {
    let store = registry()?;
    match command {
        ProjectCommand::List { json } => print_projects(&store.load()?, json),
        ProjectCommand::Add {
            name,
            working_dir,
            revision,
            json,
        } => {
            let revision = revision.unwrap_or(store.load()?.revision);
            let registry = store.add(revision, Project { name, working_dir })?;
            print_projects(&registry, json)
        }
        ProjectCommand::Remove {
            name,
            revision,
            json,
        } => {
            let revision = revision.unwrap_or(store.load()?.revision);
            let registry = store.remove(revision, &name)?;
            print_projects(&registry, json)
        }
    }
}

fn run_routine(command: RoutineCommand) -> Result<()> {
    match command {
        RoutineCommand::List {
            project,
            filter,
            trigger,
            event_kind,
            json,
        } => {
            let projects = registry()?.select(&project, filter.as_deref())?;
            let mut rows = Vec::with_capacity(projects.len());
            for project in projects {
                let response = send(&project.working_dir, Action::List)?;
                let Response::Routines { revision, routines } = response else {
                    bail!("daemon returned an unexpected list response");
                };
                let routines = routines
                    .into_iter()
                    .filter(|view| match trigger {
                        Some(TriggerSelection::Cron) => {
                            matches!(view.routine.trigger, Trigger::Cron(_))
                        }
                        Some(TriggerSelection::Event) => {
                            matches!(view.routine.trigger, Trigger::Event { .. })
                        }
                        None => true,
                    })
                    .filter(|view| {
                        event_kind.as_ref().is_none_or(|expected| {
                            matches!(&view.routine.trigger, Trigger::Event { kind } if kind == expected)
                        })
                    })
                    .collect();
                rows.push((project, revision, routines));
            }
            print_routine_list(rows, json)
        }
        RoutineCommand::Show {
            name,
            project,
            json,
        } => {
            let project = resolve_project(&project)?;
            print_response(send(&project.working_dir, Action::Show { name })?, json)
        }
        RoutineCommand::Add {
            name,
            cron,
            event,
            argv,
            prompt,
            project,
            revision,
            disabled,
            json,
        } => {
            let project = resolve_project(&project)?;
            let revision = revision.unwrap_or(fetch_revision(&project.working_dir)?);
            print_response(
                send(
                    &project.working_dir,
                    Action::Add {
                        revision,
                        routine: Routine {
                            name,
                            trigger: trigger_from_args(cron, event)?,
                            command: argv,
                            prompt,
                            enabled: !disabled,
                        },
                    },
                )?,
                json,
            )
        }
        RoutineCommand::Edit {
            name,
            new_name,
            cron,
            event,
            argv,
            prompt,
            project,
            revision,
            json,
        } => {
            let project = resolve_project(&project)?;
            let (actual_revision, current) = fetch_routine(&project.working_dir, &name)?;
            let revision = revision.unwrap_or(actual_revision);
            print_response(
                send(
                    &project.working_dir,
                    Action::Edit {
                        revision,
                        old_name: name.clone(),
                        routine: Routine {
                            name: new_name.unwrap_or(name),
                            trigger: trigger_from_args(cron, event)?,
                            command: argv,
                            prompt,
                            enabled: current.routine.enabled,
                        },
                    },
                )?,
                json,
            )
        }
        RoutineCommand::Delete {
            name,
            project,
            revision,
            json,
        } => mutate_named(project, revision, json, |revision| Action::Delete {
            revision,
            name,
        }),
        RoutineCommand::Enable {
            name,
            project,
            revision,
            json,
        } => mutate_named(project, revision, json, |revision| Action::SetEnabled {
            revision,
            name,
            enabled: true,
        }),
        RoutineCommand::Disable {
            name,
            project,
            revision,
            json,
        } => mutate_named(project, revision, json, |revision| Action::SetEnabled {
            revision,
            name,
            enabled: false,
        }),
        RoutineCommand::Run {
            name,
            project,
            json,
        } => send_named(project, json, Action::Run { name }),
        RoutineCommand::Fire {
            project,
            kind,
            event_id,
            payload,
            payload_file,
            json,
        } => {
            let payload = match (payload, payload_file) {
                (Some(payload), None) => payload,
                (None, Some(path)) => fs::read_to_string(&path)
                    .with_context(|| format!("reading event payload {}", path.display()))?,
                _ => bail!("exactly one of --payload or --payload-file is required"),
            };
            let payload = serde_json::from_str(&payload).context("parsing event payload JSON")?;
            let project = resolve_project(&project)?;
            print_response(
                send(
                    &project.working_dir,
                    Action::Fire {
                        kind,
                        payload,
                        event_id,
                    },
                )?,
                json,
            )
        }
        RoutineCommand::Cancel {
            name,
            project,
            json,
        } => send_named(project, json, Action::Cancel { name }),
        RoutineCommand::Logs {
            name,
            project,
            json,
        } => send_named(project, json, Action::Logs { name }),
    }
}

fn mutate_named(
    project_name: String,
    revision: Option<u64>,
    json: bool,
    action: impl FnOnce(u64) -> Action,
) -> Result<()> {
    let project = resolve_project(&project_name)?;
    let revision = revision.unwrap_or(fetch_revision(&project.working_dir)?);
    print_response(send(&project.working_dir, action(revision))?, json)
}

fn send_named(project_name: String, json: bool, action: Action) -> Result<()> {
    let project = resolve_project(&project_name)?;
    print_response(send(&project.working_dir, action)?, json)
}

fn run_daemon(command: DaemonCommand) -> Result<()> {
    let client = client()?;
    match command {
        DaemonCommand::Start => {
            client.start(daemon_command()?)?;
            println!("asched daemon running");
            Ok(())
        }
        DaemonCommand::Status { json } => print_response(
            client.request(&Request::new(PathBuf::new(), Action::Status))?,
            json,
        ),
        DaemonCommand::Stop { json } => print_response(
            client.request(&Request::new(PathBuf::new(), Action::Shutdown))?,
            json,
        ),
    }
}

fn serve_daemon() -> Result<()> {
    let root = RegistryStore::default_root()?;
    if let Some(mut startup) = startup_notifier()? {
        asched_core::routine::daemon::serve_with_startup(root, move |result| {
            let message = match result {
                Ok(()) => "ready".to_string(),
                Err(error) => format!("error:{error}"),
            };
            let _ = startup.write_all(message.as_bytes());
        })?;
    } else {
        asched_core::routine::daemon::serve(root)?;
    }
    Ok(())
}

fn startup_notifier() -> Result<Option<File>> {
    let Some(raw) = std::env::var_os(STARTUP_FD_ENV) else {
        return Ok(None);
    };
    std::env::remove_var(STARTUP_FD_ENV);
    let descriptor = validate_startup_descriptor(&raw)?;
    Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
}

fn validate_startup_descriptor(raw: &OsStr) -> Result<i32> {
    let descriptor = raw
        .to_string_lossy()
        .parse::<i32>()
        .context("invalid asched startup descriptor")?;
    if descriptor < 3 || unsafe { libc::fcntl(descriptor, libc::F_GETFD) } == -1 {
        bail!("invalid asched startup descriptor");
    }
    Ok(descriptor)
}

fn fetch_revision(working_dir: &Path) -> Result<u64> {
    match send(working_dir, Action::List)? {
        Response::Routines { revision, .. } => Ok(revision),
        _ => bail!("daemon returned an unexpected revision response"),
    }
}

fn fetch_routine(working_dir: &Path, name: &str) -> Result<(u64, RoutineView)> {
    match send(working_dir, Action::Show { name: name.into() })? {
        Response::Routine { revision, routine } => Ok((revision, *routine)),
        _ => bail!("daemon returned an unexpected show response"),
    }
}

fn print_projects(registry: &asched_core::ProjectRegistry, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(registry)?);
        return Ok(());
    }
    println!("revision {}", registry.revision);
    for project in &registry.projects {
        println!(
            "{}\t{}",
            terminal_safe(&project.name),
            terminal_safe(&project.working_dir.to_string_lossy())
        );
    }
    Ok(())
}

fn print_routine_list(
    rows: Vec<(Project, u64, Vec<asched_core::routine::ipc::RoutineView>)>,
    as_json: bool,
) -> Result<()> {
    if as_json {
        let rows = rows
            .into_iter()
            .map(|(project, revision, routines)| {
                json!({
                    "project": project,
                    "revision": revision,
                    "routines": routines,
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for (project, revision, routines) in rows {
        for view in routines {
            println!(
                "{}\t{}\t{}\t{}\t{}\tlatest={}\t{}\trevision={}",
                terminal_safe(&project.name),
                terminal_safe(&view.routine.name),
                if view.routine.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                if view.capabilities.can_cancel {
                    "running"
                } else {
                    "idle"
                },
                terminal_safe(&format_trigger(&view.routine.trigger)),
                view.latest_run
                    .as_ref()
                    .map(|run| format!("{:?}", run.status))
                    .unwrap_or_else(|| "never".into()),
                format_argv(&view.routine.command),
                revision
            );
        }
    }
    Ok(())
}

fn print_response(response: Response, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    match response {
        Response::Routines { revision, routines } => {
            println!("revision {revision}");
            for view in routines {
                println!(
                    "{}\t{}\t{}\t{}\tlatest={}\t{}",
                    terminal_safe(&view.routine.name),
                    if view.routine.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if view.capabilities.can_cancel {
                        "running"
                    } else {
                        "idle"
                    },
                    terminal_safe(&format_trigger(&view.routine.trigger)),
                    view.latest_run
                        .as_ref()
                        .map(|run| format!("{:?}", run.status))
                        .unwrap_or_else(|| "never".into()),
                    format_argv(&view.routine.command)
                );
            }
        }
        Response::Routine { revision, routine } => println!(
            "revision {revision}\nname: {}\nenabled: {}\ntrigger: {}\ncommand: {}\nprompt: {}",
            terminal_safe(&routine.routine.name),
            routine.routine.enabled,
            terminal_safe(&format_trigger(&routine.routine.trigger)),
            format_argv(&routine.routine.command),
            terminal_safe(&routine.routine.prompt)
        ),
        Response::Runs { runs } => {
            for run in runs {
                println!(
                    "{}\t{:?}\t{}",
                    terminal_safe(&run.id),
                    run.status,
                    terminal_safe(&run.final_output)
                );
            }
        }
        Response::Fire { outcome } => match outcome {
            FireOutcome::Handled { routines } => {
                for routine in routines {
                    match routine {
                        RoutineFire::Started { name } => println!("{name}\tstarted"),
                        RoutineFire::AlreadyRunning { name } => {
                            println!("{name}\talready_running")
                        }
                    }
                }
            }
            FireOutcome::Deduplicated => println!("deduplicated"),
            FireOutcome::NoMatch => println!("no_match"),
        },
        Response::Daemon { protocol, pid } => {
            println!("asched daemon pid={pid} protocol={protocol}")
        }
        Response::Ok { revision } => match revision {
            Some(revision) => println!("ok revision={revision}"),
            None => println!("ok"),
        },
        Response::Error { kind, message } => bail!("{kind}: {message}"),
    }
    Ok(())
}

fn trigger_from_args(cron: Option<String>, event: Option<String>) -> Result<Trigger> {
    match (cron, event) {
        (Some(expression), None) => Ok(Trigger::Cron(expression)),
        (None, Some(kind)) => Ok(Trigger::Event { kind }),
        _ => bail!("exactly one of --cron or --event is required"),
    }
}

fn format_trigger(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Cron(expression) => format!("cron:{expression}"),
        Trigger::Event { kind } => format!("event:{kind}"),
    }
}

fn format_argv(argv: &[String]) -> String {
    serde_json::to_string(argv).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
#[path = "cli_contract_tests.rs"]
mod cli_contract_tests;
