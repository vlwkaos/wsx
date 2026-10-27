use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::runtime_util::{
    api_error, expect_ack, expect_snapshot, print_json, unexpected_response,
};
use crate::session_state;
use wsx_core::{
    config::global::{project_matches_group, GlobalConfig, GroupKey},
    model::workspace::{Project, WorkspaceState},
    ops, runtime,
};

mod agent;
mod args;
mod daemon;
mod exchange;
mod group;
mod routine;
mod session;
mod workspace_cmds;

pub(crate) use agent::*;
pub use args::*;
use daemon::*;
use exchange::*;
use group::*;
pub(crate) use routine::*;
use session::*;
pub(crate) use workspace_cmds::*;

pub fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Status {
            output:
                ListOutput {
                    json,
                    format,
                    group,
                },
        } => cmd_status(json, format, group.as_ref()),
        Command::Worktree { subcommand } => match subcommand {
            WorktreeCmd::Create {
                branch,
                project,
                from,
                json,
            } => cmd_worktree_create(&branch, project.as_deref(), from.as_deref(), json),
            WorktreeCmd::Delete { branch, project } => {
                cmd_worktree_delete(&branch, project.as_deref())
            }
            WorktreeCmd::List {
                project,
                output:
                    ListOutput {
                        json,
                        format,
                        group,
                    },
            } => cmd_worktree_list(project.as_deref(), json, format, group.as_ref()),
        },
        Command::Session { subcommand } => match subcommand {
            SessionCmd::Create {
                name,
                command,
                json,
                scope,
            } => cmd_session_create(scope, name, command, json),
            SessionCmd::Current { json } => cmd_session_current(json),
            SessionCmd::Delete {
                session,
                json,
                scope,
            } => cmd_session_delete(&session, &scope, json),
            SessionCmd::Restart {
                session,
                json,
                scope,
            } => cmd_session_restart(&session, json, &scope),
            SessionCmd::SendKeys {
                session: s,
                keys,
                no_enter,
                scope,
            } => cmd_session_send_text(&s, &keys, no_enter, &scope),
            SessionCmd::SendText {
                session,
                text,
                no_enter,
                scope,
            } => cmd_session_send_text(&session, &text, no_enter, &scope),
            SessionCmd::Prompt {
                session,
                prompt,
                scope,
            } => cmd_session_prompt(&session, prompt, &scope),
            SessionCmd::Peek {
                session: s,
                lines,
                trim,
                agent,
                scope,
            } => cmd_session_peek(&s, lines, trim, agent, &scope),
            SessionCmd::Rename {
                old,
                new_name,
                scope,
            } => cmd_session_rename(&old, &new_name, &scope),
            SessionCmd::List {
                project,
                output:
                    ListOutput {
                        json,
                        format,
                        group,
                    },
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
            AgentCmd::Context {
                session,
                provider,
                limit,
                messages,
                bytes,
                metadata_only,
                json,
                scope,
            } => cmd_agent_context(
                session.as_deref(),
                provider.as_deref(),
                ContextOptions {
                    limit,
                    messages,
                    bytes,
                    metadata_only,
                    json,
                },
                &scope,
            ),
            AgentCmd::Install { integration } => cmd_agent_install(integration),
            AgentCmd::Detach => cmd_agent_detach(),
            AgentCmd::Report {
                pane,
                provider,
                state,
                conversation_id,
                session_id,
                session_path,
                transcript_path,
                wake_token,
                prompt,
                resume,
                lifecycle,
                detached,
                presence_id,
                escape_interrupts,
                exchange_receipts,
                prompt_receipts,
                received,
                compacting,
            } => cmd_agent_report(
                &pane,
                AgentReportOptions {
                    provider,
                    state: state.into(),
                    attached: !detached,
                    presence_id,
                    conversation_id,
                    session_id,
                    session_path,
                    transcript_path,
                    wake_token,
                    capabilities: runtime::AgentCapabilities {
                        prompt,
                        resume,
                        lifecycle,
                        escape_interrupts,
                        exchange_receipts,
                        prompt_receipts,
                    },
                    received,
                    compacting,
                },
            ),
            AgentCmd::PresenceRenew { pane, presence_id } => {
                cmd_agent_presence_renew(&pane, &presence_id)
            }
            AgentCmd::WakeHeartbeat { pane, prompt_id } => {
                cmd_agent_wake_heartbeat(&pane, &prompt_id)
            }
            AgentCmd::Request {
                session,
                prompt,
                timeout,
                writer,
                write_claims,
                stash_draft,
                key,
                json,
                scope,
            } => cmd_agent_exchange_request(
                &session,
                prompt,
                timeout,
                writer,
                write_claims,
                stash_draft,
                key,
                json,
                &scope,
            ),
            AgentCmd::Start(args) => cmd_agent_start(args),
            AgentCmd::ExchangeReceipt {
                exchange,
                round,
                receipt,
                delivery_sha256,
                input_id,
                json,
            } => cmd_agent_exchange_receipt(
                exchange,
                round,
                receipt,
                delivery_sha256,
                input_id,
                json,
            ),
            AgentCmd::Inspect {
                exchange,
                frame,
                json,
            } => cmd_agent_exchange_inspect(exchange, frame, json),
            AgentCmd::Wait {
                exchanges,
                any,
                timeout,
                frame,
                result,
                result_bytes,
                json,
                scope,
            } => cmd_agent_exchange_wait(
                exchanges,
                any,
                timeout,
                frame,
                result.then_some(result_bytes as usize),
                json,
                &scope,
            ),
            AgentCmd::Continue {
                exchange,
                prompt,
                stash_draft,
                key,
                timeout,
                json,
            } => cmd_agent_exchange_continue(exchange, prompt, timeout, stash_draft, key, json),
            AgentCmd::Cancel { exchange, json } => cmd_agent_exchange_cancel(exchange, json),
            AgentCmd::Exchanges {
                session,
                json,
                scope,
            } => cmd_agent_exchange_list(session.as_deref(), json, &scope),
        },
        Command::Plugin { subcommand } => match subcommand {
            PluginCmd::List { json } => cmd_plugin_list(json),
            PluginCmd::Reload => cmd_plugin_reload(),
        },
        Command::Runtime { subcommand } => match subcommand {
            RuntimeCmd::Status { json } => cmd_runtime_status(json),
        },
        Command::Daemon { subcommand } => match subcommand {
            DaemonCmd::Stop { force, json } => cmd_daemon_stop(force, json),
            DaemonCmd::Restart { json } => cmd_daemon_restart(json),
            DaemonCmd::Recover => cmd_daemon_recover(),
        },
        Command::Routine { subcommand } => cmd_routine(subcommand),
        Command::Task { subcommand } => crate::task::run(subcommand),
        Command::Web { subcommand } => match subcommand {
            WebCmd::Serve { site } => wsx_web::serve(site.named()),
            WebCmd::Pair { site } => wsx_web::pair(site.named()),
            WebCmd::Devices { site, revoke, json } => {
                wsx_web::devices(site.named(), revoke.as_deref(), json)
            }
            WebCmd::Vapid => wsx_web::vapid(),
        },
    }
}

#[cfg(test)]
mod tests;
