use super::*;

pub(super) fn cmd_plugin_list(json: bool) -> Result<()> {
    match runtime::Client::local().call(&runtime::Request::PluginList)? {
        runtime::Response::Plugins(plugins) if json => {
            print_json(&plugins)?;
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
        other => unexpected_response(other, "wsxd returned an unexpected plugin response"),
    }
}

pub(super) fn cmd_plugin_reload() -> Result<()> {
    match runtime::Client::local().call(&runtime::Request::PluginReload)? {
        runtime::Response::Plugins(plugins) => {
            println!("reloaded {} plugins", plugins.len());
            Ok(())
        }
        other => unexpected_response(other, "wsxd returned an unexpected plugin reload response"),
    }
}

// ^ docs/daemon-live-upgrade.md: a stop ends every running program, so it needs `--force`
// while sessions are live. `None` means the daemon cannot report what it runs.
pub(super) fn sessions_a_stop_would_end(client: &runtime::Client) -> Result<Option<usize>> {
    Ok(match client.status()? {
        runtime::DaemonStatus::Missing => Some(0),
        runtime::DaemonStatus::Compatible => {
            let snapshot = expect_snapshot(
                client.call(&runtime::Request::Snapshot)?,
                "wsxd returned an unexpected snapshot response",
            )?;
            Some(
                snapshot
                    .sessions
                    .iter()
                    .filter(|session| {
                        snapshot
                            .panes
                            .iter()
                            .any(|pane| pane.session_id == session.id && !pane.exited)
                    })
                    .count(),
            )
        }
        runtime::DaemonStatus::Incompatible { lifecycle, .. } => {
            lifecycle.map(|status| status.live_runtimes)
        }
    })
}

pub(super) fn cmd_daemon_stop(force: bool, json: bool) -> Result<()> {
    let client = runtime::Client::local();
    let live = if force {
        None
    } else {
        match sessions_a_stop_would_end(&client)? {
            Some(0) => None,
            live => Some(live),
        }
    };
    if let Some(live) = live {
        if json {
            println!(
                "{}",
                serde_json::json!({"stopped": false, "reason": "live_sessions", "live_sessions": live})
            );
        }
        let count = live.map_or("an unknown number of".to_string(), |live| live.to_string());
        bail!(
            "wsxd not stopped: {count} running session(s) would end. `wsx daemon restart` keeps them running. Use `wsx daemon stop --force` to end them."
        );
    }
    client.shutdown()?;
    if json {
        println!("{}", serde_json::json!({"stopped": true}));
    } else {
        println!(
            "wsxd stopped and the programs in its sessions ended; saved session commands restart on next launch"
        );
    }
    Ok(())
}

pub(super) fn cmd_daemon_restart(json: bool) -> Result<()> {
    let outcome = runtime::restart_daemon()?;
    let (state, live_runtimes, message) = match &outcome {
        runtime::DaemonRestart::NotRunning => (
            "not_running",
            0,
            "wsxd is not running; the next wsx command starts it".to_string(),
        ),
        runtime::DaemonRestart::Restarted { live_runtimes } => (
            "restarted",
            *live_runtimes,
            format!(
                "wsxd restarted as {}; {live_runtimes} running terminal(s) kept",
                runtime::WSX_VERSION
            ),
        ),
        runtime::DaemonRestart::Deferred {
            live_runtimes,
            blockers,
        } => (
            "deferred",
            *live_runtimes,
            format!(
                "wsxd restart is waiting. {} Sessions keep running. The restart then continues by itself.",
                crate::app::replacement_wait_reason(blockers)
            ),
        ),
        runtime::DaemonRestart::Unsupported { daemon_version } => (
            "unsupported",
            0,
            format!(
                "wsxd {daemon_version} cannot restart into the same build without ending its sessions; nothing was stopped."
            ),
        ),
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"state": state, "live_runtimes": live_runtimes, "message": message})
        );
    }
    if matches!(outcome, runtime::DaemonRestart::Unsupported { .. }) {
        bail!(message);
    }
    if !json {
        println!("{message}");
    }
    Ok(())
}

pub(super) fn cmd_daemon_recover() -> Result<()> {
    runtime::recover_daemon()?;
    println!("wsxd recovered");
    Ok(())
}

pub(super) fn runtime_status_state(lifecycle: Option<&runtime::DaemonLifecycle>) -> &'static str {
    match lifecycle {
        Some(status) if status.phase == runtime::DaemonPhase::ReplacementPending => {
            "replacement_deferred"
        }
        Some(status) if status.daemon_revision < runtime::DAEMON_REVISION => "upgrade_available",
        _ => "ready",
    }
}

pub(super) fn incompatible_runtime_status_json(
    socket: &std::path::Path,
    daemon_protocol: Option<u32>,
    lifecycle: Option<&runtime::DaemonLifecycle>,
    lifecycle_error: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "running": true,
        "compatible": false,
        "state": "incompatible",
        "socket": socket,
        "daemon_protocol": daemon_protocol,
        "lifecycle": lifecycle,
        "lifecycle_error": lifecycle_error,
        "client": {
            "version": runtime::WSX_VERSION,
            "protocol": runtime::PROTOCOL_VERSION,
            "daemon_revision": runtime::DAEMON_REVISION,
        },
    })
}

pub(super) fn runtime_lifecycle_lines(lifecycle: &runtime::DaemonLifecycle) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Lifecycle: {:?}, version {}, daemon revision {}, {} live runtimes, {} clients, {} TUIs",
            lifecycle.phase,
            if lifecycle.version.is_empty() {
                "unknown"
            } else {
                &lifecycle.version
            },
            lifecycle.daemon_revision,
            lifecycle.live_runtimes,
            lifecycle.active_clients,
            lifecycle.active_tuis,
        ),
        format!("Binary: {}", lifecycle.binary_id),
    ];
    if let Some(target) = lifecycle.replacement_target.as_deref() {
        lines.push(format!(
            "Target: {} (version {})",
            target,
            if lifecycle.replacement_target_version.is_empty() {
                "unknown"
            } else {
                &lifecycle.replacement_target_version
            }
        ));
    }
    if !lifecycle.replacement_blockers.is_empty() {
        lines.push(format!(
            "Blockers: {}",
            lifecycle
                .replacement_blockers
                .iter()
                .map(|blocker| format!("{blocker:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // ^ docs/daemon-live-upgrade.md "A failed handoff": the daemon's stderr goes nowhere.
    if let Some(failure) = &lifecycle.last_handoff_error {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        lines.push(format!(
            "Last handoff failed {}s ago: {} failed: {}",
            now.saturating_sub(failure.unix_ms) / 1000,
            failure.step,
            failure.error
        ));
    }
    lines
}

pub(super) fn cmd_runtime_status(json: bool) -> Result<()> {
    let client = runtime::Client::local();
    match client.status() {
        Ok(runtime::DaemonStatus::Missing) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "running": false,
                        "compatible": false,
                        "state": "stopped",
                        "socket": client.socket(),
                    })
                );
            } else {
                println!("Runtime: stopped");
                println!("Socket: {}", client.socket().display());
            }
            return Ok(());
        }
        Ok(runtime::DaemonStatus::Incompatible {
            daemon_protocol,
            lifecycle,
            lifecycle_error,
        }) => {
            if json {
                print_json(&incompatible_runtime_status_json(
                    client.socket(),
                    daemon_protocol,
                    lifecycle.as_deref(),
                    lifecycle_error.as_deref(),
                ))?;
            } else {
                println!("Runtime: running (incompatible)");
                println!("Socket: {}", client.socket().display());
                println!(
                    "Protocol: daemon {}, client {}",
                    daemon_protocol
                        .map(|protocol| protocol.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    runtime::PROTOCOL_VERSION
                );
                if let Some(lifecycle) = lifecycle.as_deref() {
                    for line in runtime_lifecycle_lines(lifecycle) {
                        println!("{line}");
                    }
                }
                if let Some(error) = lifecycle_error {
                    println!("Lifecycle: unavailable ({error})");
                }
                println!("Open the matching wsx version or launch wsx to request an upgrade.");
            }
            return Ok(());
        }
        Ok(runtime::DaemonStatus::Compatible) => {}
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "running": serde_json::Value::Null,
                        "compatible": false,
                        "state": "unavailable",
                        "socket": client.socket(),
                        "error": error.to_string(),
                    })
                );
            } else {
                println!("Runtime: unavailable");
                println!("Socket: {}", client.socket().display());
                println!("Reason: {error}");
            }
            return Ok(());
        }
    }

    let snapshot = match client.call(&runtime::Request::Snapshot) {
        Ok(runtime::Response::Snapshot(snapshot)) => snapshot,
        Ok(runtime::Response::Error(error)) => return Err(api_error(error)),
        Ok(_) => bail!("wsxd returned an unexpected status response"),
        Err(error) => bail!("wsxd became unavailable while reading status: {error}"),
    };
    let lifecycle = if snapshot.capabilities.lifecycle_coordination {
        match client.call(&runtime::Request::LifecycleStatus) {
            Ok(runtime::Response::Lifecycle(status)) => Some(status),
            _ => None,
        }
    } else {
        None
    };
    let state = runtime_status_state(lifecycle.as_ref());
    if json {
        print_json(&serde_json::json!({
            "running": true,
            "compatible": true,
            "state": state,
            "socket": client.socket(),
            "protocol": snapshot.protocol,
            "epoch": snapshot.epoch,
            "revision": snapshot.revision,
            "projects": snapshot.projects.len(),
            "worktrees": snapshot.worktrees.len(),
            "sessions": snapshot.sessions.len(),
            "panes": snapshot.panes.len(),
            "capabilities": snapshot.capabilities,
            "lifecycle": lifecycle,
            "client": {
                "version": runtime::WSX_VERSION,
                "protocol": runtime::PROTOCOL_VERSION,
                "daemon_revision": runtime::DAEMON_REVISION,
            },
        }))?;
    } else {
        println!(
            "Runtime: running ({state}), protocol {}, epoch {}, revision {}",
            snapshot.protocol, snapshot.epoch, snapshot.revision
        );
        println!("Socket: {}", client.socket().display());
        if let Some(lifecycle) = lifecycle {
            for line in runtime_lifecycle_lines(&lifecycle) {
                println!("{line}");
            }
        }
        println!(
            "Resources: {} projects, {} worktrees, {} sessions, {} panes",
            snapshot.projects.len(),
            snapshot.worktrees.len(),
            snapshot.sessions.len(),
            snapshot.panes.len()
        );
    }
    Ok(())
}
