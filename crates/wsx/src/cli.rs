use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use crate::session_state;
use wsx_core::{
    config::global::GlobalConfig,
    herdr,
    model::workspace::{Project, WorkspaceState},
    ops,
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
    about = "Workspace manager — git worktrees + Herdr panes"
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
        #[arg(short = 't', long)]
        tab: Option<String>,
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
    /// Tab operations
    Tab {
        #[command(subcommand)]
        subcommand: TabCmd,
    },
    /// Inspect the required Herdr backend
    Herdr {
        #[command(subcommand)]
        subcommand: HerdrCmd,
    },
    /// Manage machine-local scheduled routines
    Routine {
        #[command(subcommand)]
        subcommand: RoutineCmd,
    },
}

#[derive(Subcommand)]
pub enum HerdrCmd {
    /// Report client, server, protocol, socket, and installed-integration health
    Status {
        #[arg(long)]
        json: bool,
    },
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
pub enum TabCmd {
    /// List tabs
    Ls,
    /// Create a tab
    Create { name: String },
    /// Rename a tab
    Rename { old: String, new_name: String },
    /// Assign a project to a tab
    Own { tab: String, project: String },
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
        #[arg(short = 't', long)]
        tab: Option<String>,
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
    /// Submit a prompt through Herdr's agent-aware input path
    Prompt { session: String, prompt: String },
    /// Peek at pane output (supports scrollback windowing)
    Peek {
        session: String,
        /// Number of lines to capture from scrollback (default: visible viewport)
        #[arg(short = 'n', long)]
        lines: Option<u32>,
        /// Lines from the bottom to skip (scroll back further)
        #[arg(short = 'o', long, default_value = "0")]
        offset: u32,
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
        #[arg(short = 't', long)]
        tab: Option<String>,
    },
}

pub fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Status { json, format, tab } => cmd_status(json, format, tab.as_deref()),
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
                tab,
            } => cmd_worktree_list(project.as_deref(), json, format, tab.as_deref()),
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
                offset,
                trim,
                agent,
            } => cmd_session_peek(&s, lines, offset, trim, agent),
            SessionCmd::Rename { old, new_name } => cmd_session_rename(&old, &new_name),
            SessionCmd::List {
                project,
                json,
                format,
                tab,
            } => cmd_session_list(project.as_deref(), json, format, tab.as_deref()),
        },
        Command::Tab { subcommand } => match subcommand {
            TabCmd::Ls => cmd_tab_ls(),
            TabCmd::Create { name } => cmd_tab_create(&name),
            TabCmd::Rename { old, new_name } => cmd_tab_rename(&old, &new_name),
            TabCmd::Own { tab, project } => cmd_tab_own(&tab, &project),
        },
        Command::Herdr { subcommand } => match subcommand {
            HerdrCmd::Status { json } => cmd_herdr_status(json),
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
    let mut workspace = ops::load_workspace(&config);
    ops::refresh_workspace(&mut workspace, &config)?;
    Ok((config, workspace))
}

/// CLI name for the default (unassigned) tab. Maps to `tab: None` in config.
const DEFAULT_TAB: &str = "default";

/// Resolve projects by project name and/or tab. `-p` takes precedence over `--tab`.
/// `--tab default` matches projects with no tab assigned.
fn resolve_projects<'a>(
    config: &GlobalConfig,
    workspace: &'a WorkspaceState,
    project_name: Option<&str>,
    tab: Option<&str>,
) -> Result<Vec<&'a Project>> {
    if let Some(n) = project_name {
        return Ok(vec![resolve_project(workspace, Some(n))?]);
    }
    let Some(tab) = tab else {
        return Ok(workspace.projects.iter().collect());
    };
    let want_default = tab == DEFAULT_TAB;
    Ok(workspace
        .projects
        .iter()
        .filter(|p| {
            let assigned = config.project_tab(&p.path);
            if want_default {
                assigned.is_none()
            } else {
                assigned == Some(tab)
            }
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

fn cmd_status(json: bool, format: Format, tab: Option<&str>) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, None, tab)?;
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
    let show_tabs = !config.tabs.is_empty();
    for project in &projects {
        if show_tabs {
            let tab = config.project_tab(&project.path).unwrap_or(DEFAULT_TAB);
            println!("[{}] {} {}", tab, project.name, project.path.display());
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
        .context("worktree created, but its initial Herdr session failed")?;
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
    tab: Option<&str>,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, project_name, tab)?;
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

fn cmd_herdr_status(json: bool) -> Result<()> {
    let report = herdr::diagnostics();
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("Herdr binary: {}", report.binary);
    match (&report.client_version, &report.client_error) {
        (Some(version), _) => println!("Client: {version}"),
        (_, Some(error)) => println!("Client: unavailable ({error})"),
        _ => println!("Client: unknown"),
    }
    if let Some(server) = report.server {
        let status = server
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let version = server
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        let protocol = server
            .get("protocol")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into());
        let socket = server
            .get("socket")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        println!("Server: {status} · version {version} · protocol {protocol}");
        println!("Socket: {socket}");
    } else if let Some(error) = report.server_error {
        println!("Server: unavailable ({error})");
    }
    if let Some(notice) = report.integration_notice {
        println!("Integrations: {notice}");
    } else {
        println!("Integrations: no installed integration updates reported");
    }
    Ok(())
}

fn cmd_session_send_text(pane_id: &str, text: &str, no_enter: bool) -> Result<()> {
    let client = herdr::Client::local()?;
    herdr::send_text_with(&client, pane_id, text, !no_enter)
}

fn cmd_session_prompt(pane_id: &str, prompt: &str) -> Result<()> {
    let client = herdr::Client::local()?;
    herdr::agent_prompt_with(&client, pane_id, prompt)
}

// ^ [[Session Peek]] Herdr read sources, bounds, ANSI, and future agent-aware reads.
fn cmd_session_peek(
    sess: &str,
    lines: Option<u32>,
    offset: u32,
    trim: bool,
    agent: bool,
) -> Result<()> {
    let requested = lines.unwrap_or(200);
    let client = herdr::Client::local()?;
    let read_lines = requested.saturating_add(offset);
    let recognized_agent = agent
        && herdr::snapshot_with(&client)?
            .panes
            .iter()
            .any(|pane| pane.pane_id == sess && pane.agent.is_some());
    let raw = if recognized_agent {
        herdr::agent_read_with(&client, sess, read_lines)?
    } else {
        herdr::read_recent_ansi_with(&client, sess, read_lines)?
    };
    let kept = raw
        .lines()
        .rev()
        .skip(offset as usize)
        .take(requested as usize)
        .collect::<Vec<_>>();
    let mut output = kept.into_iter().rev().collect::<Vec<_>>().join("\n");
    if agent && !recognized_agent {
        output = crate::ui::ansi::parse(&output)
            .lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if trim || agent {
        output = output.trim_end().to_string();
    }
    print!("{}", output);
    Ok(())
}

fn cmd_session_rename(pane_id: &str, new_label: &str) -> Result<()> {
    ops::rename_session(pane_id, new_label)?;
    println!("renamed pane {} to '{}'", pane_id, new_label);
    Ok(())
}

fn cmd_tab_ls() -> Result<()> {
    let config = load_config()?;
    for tab in &config.tabs {
        println!("{}", tab);
    }
    Ok(())
}

fn cmd_tab_create(name: &str) -> Result<()> {
    let mut config = load_config()?;
    if config.tab_exists(name) {
        bail!("tab '{}' already exists", name);
    }
    config.tabs.push(name.to_string());
    config.save()?;
    println!("created tab: {}", name);
    Ok(())
}

fn cmd_tab_rename(old: &str, new_name: &str) -> Result<()> {
    let mut config = load_config()?;
    let idx = config
        .tabs
        .iter()
        .position(|t| t == old)
        .ok_or_else(|| anyhow::anyhow!("tab '{}' not found", old))?;
    if config.tab_exists(new_name) {
        bail!("tab '{}' already exists", new_name);
    }
    config.tabs[idx] = new_name.to_string();
    for entry in &mut config.projects {
        if entry.tab.as_deref() == Some(old) {
            entry.tab = Some(new_name.to_string());
        }
    }
    config.save()?;
    println!("renamed tab: {} → {}", old, new_name);
    Ok(())
}

fn cmd_tab_own(tab: &str, project: &str) -> Result<()> {
    let mut config = load_config()?;
    let tab_opt = if tab == DEFAULT_TAB {
        None
    } else {
        if !config.tab_exists(tab) {
            bail!(
                "tab '{}' not found — use 'wsx tab create {}' first",
                tab,
                tab
            );
        }
        Some(tab.to_string())
    };
    let entry = config
        .projects
        .iter_mut()
        .find(|e| e.name == project)
        .ok_or_else(|| anyhow::anyhow!("project '{}' not found", project))?;
    entry.tab = tab_opt;
    config.save()?;
    println!("tab: {} → {}", project, tab);
    Ok(())
}

fn cmd_session_list(
    project_name: Option<&str>,
    json: bool,
    format: Format,
    tab: Option<&str>,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, project_name, tab)?;
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
