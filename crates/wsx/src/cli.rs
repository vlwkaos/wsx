use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session_state;
use wsx_core::{
    config::global::{project_matches_group, GlobalConfig, GroupKey},
    model::workspace::{Project, WorkspaceState},
    ops, runtime,
};

#[derive(Clone, Default, ValueEnum)]
pub enum Format {
    #[default]
    Normal,
    Compact,
}

#[derive(Parser)]
#[command(
    name = "wsx",
    version,
    about = "Project-first worktree and terminal manager"
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Portrait/mobile mode: full-width tree, no preview panel
    #[arg(long)]
    pub mobile: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Print workspace state
    Status {
        #[arg(long)]
        json: bool,
        #[arg(short = 'f', long, value_enum, default_value = "normal")]
        format: Format,
        #[arg(short = 'g', long = "group", value_parser = parse_group_key)]
        group: Option<GroupKey>,
    },
    /// Worktree operations
    Worktree {
        #[command(subcommand)]
        subcommand: WorktreeCmd,
    },
    /// Session operations
    Session {
        #[command(subcommand)]
        subcommand: SessionCmd,
    },
    /// Group operations
    Group {
        #[command(subcommand)]
        subcommand: GroupCmd,
    },
    /// Report normalized state from a trusted agent adapter
    Agent {
        #[command(subcommand)]
        subcommand: AgentCmd,
    },
    /// Inspect or reload trusted executable plugins
    Plugin {
        #[command(subcommand)]
        subcommand: PluginCmd,
    },
    /// Inspect the wsx terminal daemon without starting it
    Runtime {
        #[command(subcommand)]
        subcommand: RuntimeCmd,
    },
    /// Control the wsx terminal daemon without starting it
    Daemon {
        #[command(subcommand)]
        subcommand: DaemonCmd,
    },
    /// Manage machine-local scheduled routines
    Routine {
        #[command(subcommand)]
        subcommand: RoutineCmd,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum AgentStateArg {
    Unknown,
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

impl From<AgentStateArg> for runtime::AgentState {
    fn from(value: AgentStateArg) -> Self {
        match value {
            AgentStateArg::Unknown => Self::Unknown,
            AgentStateArg::Idle => Self::Idle,
            AgentStateArg::Working => Self::Working,
            AgentStateArg::Blocked => Self::Blocked,
            AgentStateArg::Done => Self::Done,
            AgentStateArg::Error => Self::Error,
        }
    }
}

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Install an agent lifecycle integration
    Install {
        #[arg(value_parser = parse_integration_target)]
        integration: wsx_core::integration::IntegrationTarget,
    },
    /// Submit an authoritative provider report for one pane
    Report {
        pane: String,
        #[arg(long)]
        provider: String,
        #[arg(long, value_enum)]
        state: AgentStateArg,
        #[arg(long, conflicts_with_all = ["session_id", "session_path"])]
        conversation_id: Option<String>,
        #[arg(long, conflicts_with = "session_path")]
        session_id: Option<String>,
        #[arg(long)]
        session_path: Option<String>,
        #[arg(long)]
        prompt: bool,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        lifecycle: bool,
    },
}

#[derive(Subcommand)]
pub enum PluginCmd {
    /// List accepted plugin manifests
    List {
        #[arg(long)]
        json: bool,
    },
    /// Reload manifests from the owner-controlled plugin directory
    Reload,
}

#[derive(Subcommand)]
pub enum RuntimeCmd {
    /// Report socket, protocol, epoch, revision, and resource counts
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// Gracefully stop wsxd; saved session commands restart on next launch
    Stop,
}

#[derive(Subcommand)]
pub enum RoutineCmd {
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Add {
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long = "arg", required = true)]
        command: Vec<String>,
        #[arg(long, default_value = "")]
        prompt: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    Edit {
        name: String,
        #[arg(long)]
        new_name: Option<String>,
        #[arg(long)]
        cron: String,
        #[arg(long = "arg", required = true)]
        command: Vec<String>,
        #[arg(long, default_value = "")]
        prompt: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    Delete {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Enable cron scheduling for a routine
    Enable {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Disable cron scheduling without cancelling an active run
    Disable {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    Run {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Cancel an active routine run
    Cancel {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    Logs {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Fire a provider-neutral event for matching routines
    Fire {
        #[arg(long)]
        kind: String,
        #[arg(long)]
        event_id: String,
        #[arg(long, default_value = "null")]
        payload: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum GroupCmd {
    /// List groups
    Ls,
    /// Create a group
    Create { name: String },
    /// Rename a group
    Rename { old: String, new_name: String },
    /// Add a project to a group
    Add { group: String, project: String },
    /// Remove a project from a group
    Remove { group: String, project: String },
}

#[derive(Subcommand)]
pub enum WorktreeCmd {
    /// Create a worktree and default session
    Create {
        branch: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    /// Delete a worktree and its sessions
    Delete {
        branch: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    /// List worktrees
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(short = 'f', long, value_enum, default_value = "normal")]
        format: Format,
        #[arg(short = 'g', long = "group", value_parser = parse_group_key)]
        group: Option<GroupKey>,
    },
}

#[derive(Subcommand)]
pub enum SessionCmd {
    /// Deprecated alias for send-text
    SendKeys {
        session: String,
        keys: String,
        #[arg(long)]
        no_enter: bool,
    },
    /// Send literal text to a terminal pane
    SendText {
        session: String,
        text: String,
        #[arg(long)]
        no_enter: bool,
    },
    /// Submit a prompt to a session's primary pane
    Prompt { session: String, prompt: String },
    /// Read a bounded window from the pane's visible semantic frame
    Peek {
        session: String,
        /// Number of visible lines to return (default: full viewport)
        #[arg(short = 'n', long)]
        lines: Option<u32>,
        /// Trim trailing blank lines
        #[arg(long)]
        trim: bool,
        /// Strip ANSI/decorations and compact for agent consumption
        #[arg(short = 'a', long)]
        agent: bool,
    },
    /// Rename a session
    Rename { old: String, new_name: String },
    /// List sessions
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(short = 'f', long, value_enum, default_value = "normal")]
        format: Format,
        #[arg(short = 'g', long = "group", value_parser = parse_group_key)]
        group: Option<GroupKey>,
    },
}

pub fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Status {
            json,
            format,
            group,
        } => cmd_status(json, format, group.as_ref()),
        Command::Worktree { subcommand } => match subcommand {
            WorktreeCmd::Create { branch, project } => {
                cmd_worktree_create(&branch, project.as_deref())
            }
            WorktreeCmd::Delete { branch, project } => {
                cmd_worktree_delete(&branch, project.as_deref())
            }
            WorktreeCmd::List {
                project,
                json,
                format,
                group,
            } => cmd_worktree_list(project.as_deref(), json, format, group.as_ref()),
        },
        Command::Session { subcommand } => match subcommand {
            SessionCmd::SendKeys {
                session: s,
                keys,
                no_enter,
            } => cmd_session_send_text(&s, &keys, no_enter),
            SessionCmd::SendText {
                session,
                text,
                no_enter,
            } => cmd_session_send_text(&session, &text, no_enter),
            SessionCmd::Prompt { session, prompt } => cmd_session_prompt(&session, &prompt),
            SessionCmd::Peek {
                session: s,
                lines,
                trim,
                agent,
            } => cmd_session_peek(&s, lines, trim, agent),
            SessionCmd::Rename { old, new_name } => cmd_session_rename(&old, &new_name),
            SessionCmd::List {
                project,
                json,
                format,
                group,
            } => cmd_session_list(project.as_deref(), json, format, group.as_ref()),
        },
        Command::Group { subcommand } => match subcommand {
            GroupCmd::Ls => cmd_group_ls(),
            GroupCmd::Create { name } => cmd_group_create(&name),
            GroupCmd::Rename { old, new_name } => cmd_group_rename(&old, &new_name),
            GroupCmd::Add { group, project } => cmd_group_add(&group, &project),
            GroupCmd::Remove { group, project } => cmd_group_remove(&group, &project),
        },
        Command::Agent { subcommand } => match subcommand {
            AgentCmd::Install { integration } => cmd_agent_install(integration),
            AgentCmd::Report {
                pane,
                provider,
                state,
                conversation_id,
                session_id,
                session_path,
                prompt,
                resume,
                lifecycle,
            } => cmd_agent_report(
                &pane,
                provider,
                state.into(),
                conversation_id,
                session_id,
                session_path,
                runtime::AgentCapabilities {
                    prompt,
                    resume,
                    lifecycle,
                },
            ),
        },
        Command::Plugin { subcommand } => match subcommand {
            PluginCmd::List { json } => cmd_plugin_list(json),
            PluginCmd::Reload => cmd_plugin_reload(),
        },
        Command::Runtime { subcommand } => match subcommand {
            RuntimeCmd::Status { json } => cmd_runtime_status(json),
        },
        Command::Daemon { subcommand } => match subcommand {
            DaemonCmd::Stop => cmd_daemon_stop(),
        },
        Command::Routine { subcommand } => cmd_routine(subcommand),
    }
}

fn cmd_routine(command: RoutineCmd) -> Result<()> {
    use asched_core::routine::{ipc::Action, Routine, Trigger};

    let (project_arg, action, json) = match command {
        RoutineCmd::List { project, json } => (project, Action::List, json),
        RoutineCmd::Show {
            name,
            project,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let (revision, routines) = fetch_routines(&path)?;
            let routine = routines
                .into_iter()
                .find(|view| view.routine.name == name)
                .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
            return print_routine_response(
                asched_core::routine::ipc::Response::Routine {
                    revision,
                    routine: Box::new(routine),
                },
                json,
            );
        }
        RoutineCmd::Add {
            name,
            cron,
            command,
            prompt,
            project,
            revision,
        } => {
            let path = routine_project(project.as_deref())?;
            let revision = revision.unwrap_or(fetch_revision(&path)?);
            let routine = Routine {
                name,
                trigger: Trigger::Cron(cron),
                command,
                prompt,
                enabled: true,
            }
            .validated()?;
            return print_routine_response(
                send_routine(&path, Action::Add { revision, routine })?,
                false,
            );
        }
        RoutineCmd::Edit {
            name,
            new_name,
            cron,
            command,
            prompt,
            project,
            revision,
        } => {
            let path = routine_project(project.as_deref())?;
            let (current_revision, routines) = fetch_routines(&path)?;
            let enabled = routines
                .iter()
                .find(|view| view.routine.name == name)
                .map(|view| view.routine.enabled)
                .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
            let routine = Routine {
                name: new_name.unwrap_or_else(|| name.clone()),
                trigger: Trigger::Cron(cron),
                command,
                prompt,
                enabled,
            }
            .validated()?;
            return print_routine_response(
                send_routine(
                    &path,
                    Action::Edit {
                        revision: revision.unwrap_or(current_revision),
                        old_name: name,
                        routine,
                    },
                )?,
                false,
            );
        }
        RoutineCmd::Delete {
            name,
            project,
            revision,
        } => {
            let path = routine_project(project.as_deref())?;
            let revision = revision.unwrap_or(fetch_revision(&path)?);
            return print_routine_response(
                send_routine(&path, Action::Delete { revision, name })?,
                false,
            );
        }
        RoutineCmd::Enable {
            name,
            project,
            revision,
        } => return set_routine_enabled(project.as_deref(), &name, revision, true),
        RoutineCmd::Disable {
            name,
            project,
            revision,
        } => return set_routine_enabled(project.as_deref(), &name, revision, false),
        RoutineCmd::Run {
            name,
            project,
            json,
        } => (project, Action::Run { name }, json),
        RoutineCmd::Cancel { name, project } => (project, Action::Cancel { name }, false),
        RoutineCmd::Logs {
            name,
            project,
            json,
        } => (project, Action::Logs { name }, json),
        RoutineCmd::Fire {
            kind,
            event_id,
            payload,
            project,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let payload = serde_json::from_str(&payload)
                .map_err(|error| anyhow::anyhow!("event payload must be JSON: {error}"))?;
            let outcome = routine_client()?.fire(&path, &kind, payload, &event_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                print_fire_outcome(&outcome);
            }
            return Ok(());
        }
    };
    let path = routine_project(project_arg.as_deref())?;
    print_routine_response(send_routine(&path, action)?, json)
}

fn routine_root() -> Result<PathBuf> {
    Ok(asched_core::registry::RegistryStore::default_root()?)
}

pub(crate) fn registered_routine_paths() -> Result<Vec<PathBuf>> {
    let registry = asched_core::registry::RegistryStore::new(routine_root()?).load()?;
    Ok(registry
        .projects
        .into_iter()
        .map(|project| project.working_dir)
        .collect())
}

fn routine_project(value: Option<&str>) -> Result<PathBuf> {
    let (config, warning) = GlobalConfig::load()?;
    if let Some(warning) = warning {
        eprintln!("warning: {warning}");
    }
    if let Some(value) = value {
        let path = PathBuf::from(value);
        if path.exists() {
            return Ok(path);
        }
        return config
            .projects
            .iter()
            .find(|p| p.name == value)
            .map(|p| p.path.clone())
            .ok_or_else(|| anyhow::anyhow!("project '{value}' not found"));
    }
    match config.projects.as_slice() {
        [project] => Ok(project.path.clone()),
        [] => Ok(std::env::current_dir()?),
        many => bail!(
            "multiple projects — use -p to specify: {}",
            many.iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn send_routine(
    project: &Path,
    action: asched_core::routine::ipc::Action,
) -> Result<asched_core::routine::ipc::Response> {
    let request = asched_core::routine::ipc::Request::new(project.to_path_buf(), action);
    // ^ asched owns daemon lifecycle: vendor/asched/README.md#architecture
    Ok(routine_client()?.request_with_start(&request, asched_daemon_command()?)?)
}

fn routine_client() -> Result<asched_core::routine::RoutineClient> {
    Ok(asched_core::routine::RoutineClient::new(routine_root()?))
}

fn asched_daemon_command() -> Result<ProcessCommand> {
    let binary = std::env::var_os("ASCHED_BIN").unwrap_or_else(|| "asched".into());
    let mut command = ProcessCommand::new(binary);
    command
        .arg("daemon-serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

fn fetch_routines(project: &Path) -> Result<(u64, Vec<asched_core::routine::ipc::RoutineView>)> {
    match send_routine(project, asched_core::routine::ipc::Action::List)? {
        asched_core::routine::ipc::Response::Routines { revision, routines } => {
            Ok((revision, routines))
        }
        other => bail!("routine daemon returned unexpected response: {other:?}"),
    }
}

fn fetch_revision(project: &Path) -> Result<u64> {
    Ok(fetch_routines(project)?.0)
}

fn set_routine_enabled(
    project: Option<&str>,
    name: &str,
    revision: Option<u64>,
    enabled: bool,
) -> Result<()> {
    let path = routine_project(project)?;
    let (current_revision, routines) = fetch_routines(&path)?;
    let mut routine = routines
        .into_iter()
        .find(|view| view.routine.name == name)
        .map(|view| view.routine)
        .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
    routine.enabled = enabled;
    print_routine_response(
        send_routine(
            &path,
            asched_core::routine::ipc::Action::Edit {
                revision: revision.unwrap_or(current_revision),
                old_name: name.to_string(),
                routine,
            },
        )?,
        false,
    )
}

fn print_fire_outcome(outcome: &asched_core::routine::FireOutcome) {
    use asched_core::routine::{FireOutcome, RoutineFire};
    match outcome {
        FireOutcome::Deduplicated => println!("deduplicated"),
        FireOutcome::NoMatch => println!("no matching routines"),
        FireOutcome::Handled { routines } => {
            for routine in routines {
                match routine {
                    RoutineFire::Started { name } => println!("started\t{name}"),
                    RoutineFire::AlreadyRunning { name } => {
                        println!("already running\t{name}")
                    }
                }
            }
        }
    }
}

fn print_routine_response(response: asched_core::routine::ipc::Response, json: bool) -> Result<()> {
    use asched_core::routine::ipc::Response;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    match response {
        Response::Routines { revision, routines } => {
            println!("revision {revision}");
            for view in routines {
                println!(
                    "{}\t{}\t{}\t{}",
                    view.routine.name,
                    if view.routine.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    format_trigger(&view.routine.trigger),
                    format_argv(&view.routine.command)
                );
            }
        }
        Response::Routine { revision, routine } => println!(
            "revision {revision}\nname: {}\nenabled: {}\ntrigger: {}\ncommand: {}\nprompt: {}",
            routine.routine.name,
            routine.routine.enabled,
            format_trigger(&routine.routine.trigger),
            format_argv(&routine.routine.command),
            routine.routine.prompt
        ),
        Response::Runs { runs } => {
            for run in runs {
                println!(
                    "{}\t{:?}\t{}",
                    run.id,
                    run.status,
                    run.final_output.replace('\n', " ")
                );
            }
        }
        Response::Fire { outcome } => print_fire_outcome(&outcome),
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

fn format_trigger(trigger: &asched_core::routine::Trigger) -> String {
    match trigger {
        asched_core::routine::Trigger::Cron(cron) => cron.clone(),
        asched_core::routine::Trigger::Event { kind } => format!("event:{kind}"),
    }
}

fn format_argv(argv: &[String]) -> String {
    serde_json::to_string(argv).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod routine_output_tests {
    use super::format_argv;

    #[test]
    fn plain_routine_argv_preserves_argument_boundaries() {
        assert_eq!(
            format_argv(&["printf".into(), "two words".into(), "".into()]),
            r#"["printf","two words",""]"#
        );
    }
}

// --- Helpers ---

fn load_config() -> Result<GlobalConfig> {
    let (config, warning) = GlobalConfig::load()?;
    if let Some(w) = warning {
        eprintln!("warning: {}", w);
    }
    Ok(config)
}

fn load_full_workspace() -> Result<(GlobalConfig, WorkspaceState)> {
    let config = load_config()?;
    let workspace = ops::load_full_workspace(&config)?;
    Ok((config, workspace))
}

fn parse_group_key(value: &str) -> std::result::Result<GroupKey, String> {
    if value.eq_ignore_ascii_case("recent") {
        Ok(GroupKey::Recent)
    } else if value.eq_ignore_ascii_case("ungrouped") {
        Ok(GroupKey::Ungrouped)
    } else {
        GroupKey::named(value)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod group_command_tests {
    use super::{Args, Command, GroupCmd};
    use clap::Parser;
    use wsx_core::config::global::GroupKey;

    #[test]
    fn canonical_group_filter_is_singular() {
        let args = Args::try_parse_from(["wsx", "status", "--group", "work"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Status { group, .. })
                if group == Some(GroupKey::Named("work".into()))
        ));
        assert!(
            Args::try_parse_from(["wsx", "status", "--group", "work", "--group", "recent"])
                .is_err()
        );
    }

    #[test]
    fn group_add_is_canonical_and_tab_surfaces_are_removed() {
        let args = Args::try_parse_from(["wsx", "group", "add", "work", "wsx"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Group {
                subcommand: GroupCmd::Add { group, project }
            }) if group == "work" && project == "wsx"
        ));
        assert!(Args::try_parse_from(["wsx", "tab", "ls"]).is_err());
        assert!(Args::try_parse_from(["wsx", "status", "--tab", "work"]).is_err());
    }
}

#[cfg(test)]
mod agent_command_tests {
    use super::{AgentCmd, Args, Command};
    use clap::Parser;
    use wsx_core::integration::IntegrationTarget;

    #[test]
    fn supported_integration_has_an_explicit_install_command() {
        let args = Args::try_parse_from(["wsx", "agent", "install", "pi"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Install {
                    integration: IntegrationTarget::Pi
                }
            })
        ));
        assert!(Args::try_parse_from(["wsx", "agent", "install", "unsupported"]).is_err());
    }

    #[test]
    fn report_accepts_session_id_without_other_session_identifiers() {
        let args = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "pi",
            "--state",
            "idle",
            "--session-id",
            "abc",
        ])
        .unwrap();

        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    session_id,
                    session_path,
                    conversation_id,
                    ..
                }
            }) if session_id == Some("abc".into())
                && session_path.is_none()
                && conversation_id.is_none()
        ));
    }

    #[test]
    fn report_accepts_session_path_without_other_session_identifiers() {
        let args = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "pi",
            "--state",
            "idle",
            "--session-path",
            "/absolute/session.jsonl",
        ])
        .unwrap();

        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    session_id,
                    session_path,
                    conversation_id,
                    ..
                }
            }) if session_path == Some("/absolute/session.jsonl".into())
                && session_id.is_none()
                && conversation_id.is_none()
        ));
    }

    #[test]
    fn report_keeps_legacy_conversation_id() {
        let args = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "pi",
            "--state",
            "idle",
            "--conversation-id",
            "legacy",
        ])
        .unwrap();

        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    conversation_id,
                    session_id,
                    session_path,
                    ..
                }
            }) if conversation_id == Some("legacy".into())
                && session_id.is_none()
                && session_path.is_none()
        ));
    }

    #[test]
    fn report_rejects_conflicting_session_identifiers() {
        let base = [
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "pi",
            "--state",
            "idle",
        ];

        for extra in [
            ["--conversation-id", "legacy", "--session-id", "abc"],
            [
                "--conversation-id",
                "legacy",
                "--session-path",
                "/absolute/session.jsonl",
            ],
            [
                "--session-id",
                "abc",
                "--session-path",
                "/absolute/session.jsonl",
            ],
        ] {
            let argv = base.into_iter().chain(extra).collect::<Vec<_>>();
            assert!(Args::try_parse_from(argv).is_err());
        }
    }
}

#[cfg(test)]
mod daemon_command_tests {
    use super::{Args, Command, DaemonCmd};
    use clap::Parser;

    #[test]
    fn daemon_stop_is_a_top_level_command() {
        let args = Args::try_parse_from(["wsx", "daemon", "stop"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Daemon {
                subcommand: DaemonCmd::Stop
            })
        ));
    }
}

/// Resolve projects by project name or one active group. `-p` takes precedence over `--group`.
// ^ [[wsx Architecture]] Workspace filtering has one optional group identity.
fn resolve_projects<'a>(
    config: &GlobalConfig,
    workspace: &'a WorkspaceState,
    project_name: Option<&str>,
    group: Option<&GroupKey>,
) -> Result<Vec<&'a Project>> {
    if let Some(name) = project_name {
        return Ok(vec![resolve_project(workspace, Some(name))?]);
    }
    if let Some(GroupKey::Named(name)) = group {
        if !config.named_group_exists(name) {
            bail!("group '{}' not found", name);
        }
    }
    let now = now_unix_ms();
    Ok(workspace
        .projects
        .iter()
        .filter(|project| {
            project_matches_group(
                config.project_groups(&project.path),
                project.last_agent_active_unix_ms,
                project.last_terminal_active_unix_ms,
                group,
                now,
            )
        })
        .collect())
}

fn resolve_project<'a>(workspace: &'a WorkspaceState, name: Option<&str>) -> Result<&'a Project> {
    match name {
        Some(n) => workspace
            .projects
            .iter()
            .find(|p| p.name == n)
            .ok_or_else(|| anyhow::anyhow!("project '{}' not found", n)),
        None => match workspace.projects.len() {
            0 => bail!("no projects registered"),
            1 => Ok(&workspace.projects[0]),
            _ => bail!(
                "multiple projects — use -p to specify: {}",
                workspace
                    .projects
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

fn activity_label(s: &wsx_core::model::workspace::SessionInfo) -> &'static str {
    session_state::status_label(s)
}

#[cfg(test)]
mod routine_client_wiring_tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn legacy_embedded_daemon_command_is_removed() {
        assert!(Args::try_parse_from(["wsx", "routine", "daemon", "start"]).is_err());
        assert!(Args::try_parse_from(["wsx", "routine-daemon-serve"]).is_err());
    }
}

fn git_label(wt: &wsx_core::model::workspace::WorktreeInfo) -> String {
    wt.git_info
        .as_ref()
        .map(|g| format!("+{}-{}", g.ahead, g.behind))
        .unwrap_or_else(|| "-".to_string())
}

fn sessions_inline(wt: &wsx_core::model::workspace::WorktreeInfo) -> String {
    wt.sessions
        .iter()
        .map(|s| format!("{}[{}]", s.pane_id, activity_label(s)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i + 1 < ncols {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }
    let mut line = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i + 1 < ncols {
            line.push_str(&format!("{:<width$}", h, width = widths[i] + 2));
        } else {
            line.push_str(h);
        }
    }
    println!("{}", line);
    for row in rows {
        line.clear();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 < ncols {
                line.push_str(&format!("{:<width$}", cell, width = widths[i] + 2));
            } else {
                line.push_str(cell);
            }
        }
        println!("{}", line);
    }
}

// --- Command implementations ---

fn cmd_status(json: bool, format: Format, group: Option<&GroupKey>) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, None, group)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&projects)?);
        return Ok(());
    }
    if let Format::Compact = format {
        let headers = &["project", "branch", "git", "sessions"];
        let rows: Vec<Vec<String>> = projects
            .iter()
            .flat_map(|p| {
                p.worktrees.iter().map(move |wt| {
                    vec![
                        p.name.clone(),
                        wt.branch.clone(),
                        git_label(wt),
                        sessions_inline(wt),
                    ]
                })
            })
            .collect();
        print_table(headers, &rows);
        return Ok(());
    }
    let show_groups = !config.groups.is_empty();
    for project in &projects {
        if show_groups {
            let memberships = config.project_groups(&project.path);
            let label = if memberships.is_empty() {
                "ungrouped".to_string()
            } else {
                memberships.join(",")
            };
            println!("[{}] {} {}", label, project.name, project.path.display());
        } else {
            println!("{} {}", project.name, project.path.display());
        }
        for wt in &project.worktrees {
            let git = wt
                .git_info
                .as_ref()
                .map(|g| format!(" [{}↑ {}↓]", g.ahead, g.behind))
                .unwrap_or_default();
            println!("  {}{} ({} sessions)", wt.branch, git, wt.sessions.len());
            for s in &wt.sessions {
                println!("    {:<40} {}", s.pane_id, activity_label(s));
            }
        }
    }
    Ok(())
}

fn cmd_worktree_create(branch: &str, project_name: Option<&str>) -> Result<()> {
    let (_, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project_name)?;
    let proj_config = project.config.clone().unwrap_or_default();
    let (wt_path, warning) =
        ops::create_worktree(&project.path, &project.default_branch, &proj_config, branch)?;
    if let Some(w) = warning {
        eprintln!("warning: {}", w);
    }
    let wt_slug = wsx_core::model::workspace::canonical_session_slug(&project.name, &wt_path);
    let (pane_id, _) = ops::create_session(&project.name, &wt_slug, &wt_path, None, None)
        .context("worktree created, but its initial wsx session failed")?;
    println!("worktree: {}", wt_path.display());
    println!("pane:     {}", pane_id);
    Ok(())
}

fn cmd_worktree_delete(branch: &str, project_name: Option<&str>) -> Result<()> {
    let (_, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project_name)?;
    let wt = project
        .worktrees
        .iter()
        .find(|w| w.branch == branch || w.alias.as_deref() == Some(branch))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "worktree for branch '{}' not found in project '{}'",
                branch,
                project.name
            )
        })?;
    ops::delete_worktree(&project.path, &wt.path, &wt.branch)?;
    println!("deleted worktree: {}", wt.path.display());
    Ok(())
}

fn cmd_worktree_list(
    project_name: Option<&str>,
    json: bool,
    format: Format,
    group: Option<&GroupKey>,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, project_name, group)?;
    if json {
        let worktrees: Vec<_> = projects.iter().flat_map(|p| p.worktrees.iter()).collect();
        println!("{}", serde_json::to_string_pretty(&worktrees)?);
        return Ok(());
    }
    if let Format::Compact = format {
        let headers = &["project", "branch", "git", "path"];
        let rows: Vec<Vec<String>> = projects
            .iter()
            .flat_map(|p| {
                p.worktrees.iter().map(move |wt| {
                    vec![
                        p.name.clone(),
                        wt.branch.clone(),
                        git_label(wt),
                        wt.path.display().to_string(),
                    ]
                })
            })
            .collect();
        print_table(headers, &rows);
        return Ok(());
    }
    for p in projects {
        for wt in &p.worktrees {
            let git = wt
                .git_info
                .as_ref()
                .map(|g| format!(" [{}↑ {}↓]", g.ahead, g.behind))
                .unwrap_or_default();
            println!("{}/{}{} — {}", p.name, wt.branch, git, wt.path.display());
        }
    }
    Ok(())
}

fn parse_integration_target(
    value: &str,
) -> std::result::Result<wsx_core::integration::IntegrationTarget, String> {
    value.parse()
}

fn cmd_agent_install(integration: wsx_core::integration::IntegrationTarget) -> Result<()> {
    let installed = wsx_core::integration::install(integration)?;
    println!("installed {} agent integration", integration.label());
    for path in installed.paths {
        println!("  {}", path.display());
    }
    Ok(())
}

fn cmd_agent_report(
    selector: &str,
    provider: String,
    state: runtime::AgentState,
    conversation_id: Option<String>,
    session_id: Option<String>,
    session_path: Option<String>,
    capabilities: runtime::AgentCapabilities,
) -> Result<()> {
    let pane_id = resolve_pane(selector)?;
    let session_ref = if let Some(value) = session_path {
        Some(
            runtime::AgentSessionRef::path(value)
                .ok_or_else(|| anyhow::anyhow!("invalid absolute agent session path"))?,
        )
    } else if let Some(value) = session_id {
        Some(
            runtime::AgentSessionRef::id(value)
                .ok_or_else(|| anyhow::anyhow!("invalid agent session ID"))?,
        )
    } else {
        None
    };
    let conversation_id = conversation_id.or_else(|| {
        session_ref
            .as_ref()
            .map(|session_ref| session_ref.value.clone())
    });
    match runtime::Client::local().call(&runtime::Request::AgentReport {
        pane_id,
        provider,
        state,
        conversation_id,
        session_ref,
        capabilities,
    })? {
        runtime::Response::Ack { revision } => {
            println!("agent report accepted at revision {revision}");
            Ok(())
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected agent response"),
    }
}

fn cmd_plugin_list(json: bool) -> Result<()> {
    match runtime::Client::local().call(&runtime::Request::PluginList)? {
        runtime::Response::Plugins(plugins) if json => {
            println!("{}", serde_json::to_string_pretty(&plugins)?);
            Ok(())
        }
        runtime::Response::Plugins(plugins) => {
            for plugin in plugins {
                println!(
                    "{}\t{}\t{}",
                    plugin.id,
                    if plugin.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    plugin.name
                );
            }
            Ok(())
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected plugin response"),
    }
}

fn cmd_plugin_reload() -> Result<()> {
    match runtime::Client::local().call(&runtime::Request::PluginReload)? {
        runtime::Response::Plugins(plugins) => {
            println!("reloaded {} plugins", plugins.len());
            Ok(())
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected plugin reload response"),
    }
}

fn cmd_daemon_stop() -> Result<()> {
    runtime::Client::local().shutdown()?;
    println!("wsxd stopped; saved session commands restart on next launch");
    Ok(())
}

fn cmd_runtime_status(json: bool) -> Result<()> {
    let client = runtime::Client::local();
    let snapshot = match client.call(&runtime::Request::Snapshot) {
        Ok(runtime::Response::Snapshot(snapshot)) => snapshot,
        Ok(runtime::Response::Error(error)) => bail!("{}: {}", error.code, error.message),
        Ok(_) => bail!("wsxd returned an unexpected status response"),
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "running": false,
                        "socket": client.socket(),
                        "error": error.to_string(),
                    })
                );
                return Ok(());
            }
            println!("Runtime: stopped");
            println!("Socket: {}", client.socket().display());
            println!("Reason: {error}");
            return Ok(());
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "running": true,
                "socket": client.socket(),
                "protocol": snapshot.protocol,
                "epoch": snapshot.epoch,
                "revision": snapshot.revision,
                "projects": snapshot.projects.len(),
                "worktrees": snapshot.worktrees.len(),
                "sessions": snapshot.sessions.len(),
                "panes": snapshot.panes.len(),
                "capabilities": snapshot.capabilities,
            }))?
        );
    } else {
        println!(
            "Runtime: running · protocol {} · epoch {} · revision {}",
            snapshot.protocol, snapshot.epoch, snapshot.revision
        );
        println!("Socket: {}", client.socket().display());
        println!(
            "Resources: {} projects · {} worktrees · {} sessions · {} panes",
            snapshot.projects.len(),
            snapshot.worktrees.len(),
            snapshot.sessions.len(),
            snapshot.panes.len()
        );
    }
    Ok(())
}

fn resolve_pane(selector: &str) -> Result<runtime::PaneId> {
    let snapshot = ops::runtime_snapshot()?;
    if let Ok(pane_id) = selector.parse::<runtime::PaneId>() {
        if snapshot.panes.iter().any(|pane| pane.id == pane_id) {
            return Ok(pane_id);
        }
    }
    if let Ok(session_id) = selector.parse::<runtime::SessionId>() {
        if let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        {
            return Ok(session.focused_pane);
        }
    }
    let matches = snapshot
        .sessions
        .iter()
        .filter(|session| session.label == selector)
        .map(|session| session.focused_pane)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [pane_id] => Ok(*pane_id),
        [] => bail!("session or pane not found: {selector}"),
        _ => bail!("session label is ambiguous: {selector}; use a numeric session or pane ID"),
    }
}

fn send_pane_bytes(pane_id: runtime::PaneId, bytes: Vec<u8>) -> Result<()> {
    let client = runtime::Client::local();
    let client_id = runtime::new_client_id();
    match client.call(&runtime::Request::TerminalAcquire {
        pane_id,
        client_id,
        takeover: false,
    })? {
        runtime::Response::Ack { .. } => {}
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected lease response"),
    }
    let result = match client.call(&runtime::Request::TerminalInput {
        pane_id,
        client_id,
        bytes,
    })? {
        runtime::Response::Ack { .. } => Ok(()),
        runtime::Response::Error(error) => {
            Err(anyhow::anyhow!("{}: {}", error.code, error.message))
        }
        _ => Err(anyhow::anyhow!(
            "wsxd returned an unexpected input response"
        )),
    };
    let _ = client.call(&runtime::Request::TerminalRelease { pane_id, client_id });
    result
}

fn cmd_session_send_text(selector: &str, text: &str, no_enter: bool) -> Result<()> {
    let pane_id = resolve_pane(selector)?;
    let mut bytes = text.as_bytes().to_vec();
    if !no_enter {
        bytes.push(b'\r');
    }
    send_pane_bytes(pane_id, bytes)
}

fn cmd_session_prompt(selector: &str, prompt: &str) -> Result<()> {
    cmd_session_send_text(selector, prompt, false)
}

// ^ [[Session Peek]] Reads are bounded projections of authoritative semantic frames.
fn cmd_session_peek(selector: &str, lines: Option<u32>, trim: bool, agent: bool) -> Result<()> {
    let pane_id = resolve_pane(selector)?;
    let client = runtime::Client::local();
    let frame = match client.call(&runtime::Request::View {
        pane_ids: vec![pane_id],
    })? {
        runtime::Response::View { mut frames, .. } => {
            frames.pop().context("terminal frame is unavailable")?
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected view response"),
    };
    let mut rendered = frame
        .cells
        .chunks(usize::from(frame.cols))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>();
    let requested = lines.unwrap_or(u32::from(frame.rows));
    let keep_end = rendered.len();
    let keep_start = keep_end.saturating_sub(requested as usize);
    rendered = rendered[keep_start..keep_end].to_vec();
    let mut output = rendered.join("\n");
    if trim || agent {
        output = output.trim_end().to_string();
    }
    print!("{}", output);
    Ok(())
}

fn cmd_session_rename(session_id: &str, new_label: &str) -> Result<()> {
    let session_id = session_id.parse::<wsx_core::runtime::SessionId>()?;
    ops::rename_session(session_id, new_label)?;
    println!("renamed session {} to '{}'", session_id, new_label);
    Ok(())
}

fn cmd_group_ls() -> Result<()> {
    let config = load_config()?;
    for group in &config.groups {
        println!("{}", group);
    }
    Ok(())
}

fn validate_new_group(config: &GlobalConfig, name: &str) -> Result<()> {
    GroupKey::named(name.to_string()).map_err(anyhow::Error::msg)?;
    if config.named_group_exists(name) {
        bail!("group '{}' already exists", name);
    }
    Ok(())
}

fn cmd_group_create(name: &str) -> Result<()> {
    let mut config = load_config()?;
    validate_new_group(&config, name)?;
    config.groups.push(name.to_string());
    config.save()?;
    println!("created group: {}", name);
    Ok(())
}

fn cmd_group_rename(old: &str, new_name: &str) -> Result<()> {
    let mut config = load_config()?;
    let index = config
        .groups
        .iter()
        .position(|group| group == old)
        .ok_or_else(|| anyhow::anyhow!("group '{}' not found", old))?;
    validate_new_group(&config, new_name)?;
    config.groups[index] = new_name.to_string();
    for entry in &mut config.projects {
        for group in &mut entry.groups {
            if group == old {
                *group = new_name.to_string();
            }
        }
    }
    config.save()?;
    println!("renamed group: {} → {}", old, new_name);
    Ok(())
}

fn project_path(config: &GlobalConfig, project: &str) -> Result<PathBuf> {
    config
        .projects
        .iter()
        .find(|entry| entry.name == project)
        .map(|entry| entry.path.clone())
        .ok_or_else(|| anyhow::anyhow!("project '{}' not found", project))
}

fn cmd_group_add(group: &str, project: &str) -> Result<()> {
    let mut config = load_config()?;
    GroupKey::named(group.to_string()).map_err(anyhow::Error::msg)?;
    if !config.named_group_exists(group) {
        bail!("group '{}' not found", group);
    }
    let path = project_path(&config, project)?;
    if config
        .project_groups(&path)
        .iter()
        .any(|name| name == group)
    {
        println!("project '{}' is already in group '{}'", project, group);
        return Ok(());
    }
    config.add_project_to_group(&path, group);
    config.save()?;
    println!("added '{}' to group '{}'", project, group);
    Ok(())
}

fn cmd_group_remove(group: &str, project: &str) -> Result<()> {
    let mut config = load_config()?;
    GroupKey::named(group.to_string()).map_err(anyhow::Error::msg)?;
    if !config.named_group_exists(group) {
        bail!("group '{}' not found", group);
    }
    let path = project_path(&config, project)?;
    if !config.remove_project_from_group(&path, group) {
        println!("project '{}' is not in group '{}'", project, group);
        return Ok(());
    }
    config.save()?;
    println!("removed '{}' from group '{}'", project, group);
    Ok(())
}

fn cmd_session_list(
    project_name: Option<&str>,
    json: bool,
    format: Format,
    group: Option<&GroupKey>,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, project_name, group)?;
    if json {
        let sessions: Vec<_> = projects
            .iter()
            .flat_map(|p| p.worktrees.iter().flat_map(|w| w.sessions.iter()))
            .collect();
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }
    if let Format::Compact = format {
        let headers = &["project", "branch", "sessions"];
        let rows: Vec<Vec<String>> = projects
            .iter()
            .flat_map(|p| {
                p.worktrees
                    .iter()
                    .map(move |wt| vec![p.name.clone(), wt.branch.clone(), sessions_inline(wt)])
            })
            .collect();
        print_table(headers, &rows);
        return Ok(());
    }
    for p in projects {
        for wt in &p.worktrees {
            for s in &wt.sessions {
                println!(
                    "{}/{} — {} ({})",
                    p.name,
                    wt.branch,
                    s.pane_id,
                    activity_label(s)
                );
            }
        }
    }
    Ok(())
}
