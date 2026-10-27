#[cfg(test)]
mod routine_output_tests {
    use crate::cli::{format_argv, Args, Command, RoutineCmd};
    use clap::Parser;

    #[test]
    fn plain_routine_argv_preserves_argument_boundaries() {
        assert_eq!(
            format_argv(&["printf".into(), "two words".into(), "".into()]),
            r#"["printf","two words",""]"#
        );
    }

    #[test]
    fn pi_routine_cli_accepts_flag_argv_as_explicit_values() {
        let parsed = Args::try_parse_from([
            "wsx",
            "routine",
            "add",
            "weekday-review",
            "--cron",
            "0 9 * * 1-5",
            "--arg=pi",
            "--arg=-p",
            "--arg={prompt}",
            "--prompt",
            "Review the project",
        ])
        .unwrap();

        let Some(Command::Routine {
            subcommand:
                RoutineCmd::Add {
                    name,
                    command,
                    prompt,
                    ..
                },
        }) = parsed.command
        else {
            panic!("expected routine add command");
        };
        assert_eq!(name, "weekday-review");
        assert_eq!(command, ["pi", "-p", "{prompt}"]);
        assert_eq!(prompt, "Review the project");
    }
}

#[cfg(test)]
mod group_command_tests {
    use crate::cli::{resolve_projects, Args, Command, GroupCmd};
    use clap::Parser;
    use wsx_core::config::global::{GlobalConfig, GroupKey};
    use wsx_core::model::workspace::WorkspaceState;

    #[test]
    fn group_filter_accepts_named_recent_and_rejects_duplicates() {
        let args = Args::try_parse_from(["wsx", "status", "--group", "recent"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Status { output })
                if output.group == Some(GroupKey::Named("recent".into()))
        ));
        assert!(
            Args::try_parse_from(["wsx", "status", "--group", "work", "--group", "recent"])
                .is_err()
        );
        let ungrouped = Args::try_parse_from(["wsx", "status", "--group", "UNGROUPED"]).unwrap();
        assert!(matches!(
            ungrouped.command,
            Some(Command::Status { output }) if output.group == Some(GroupKey::Ungrouped)
        ));
        assert!(Args::try_parse_from(["wsx", "status", "--group", "Default"]).is_err());
    }

    #[test]
    fn named_group_filter_requires_configured_group_including_recent() {
        let workspace = WorkspaceState { projects: vec![] };
        let recent = GroupKey::Named("recent".into());
        let config = GlobalConfig::default();
        let error = resolve_projects(&config, &workspace, None, Some(&recent)).unwrap_err();
        assert!(error.to_string().contains("group 'recent' not found"));

        let mut config = GlobalConfig::default();
        config.groups.push("recent".into());
        assert!(resolve_projects(&config, &workspace, None, Some(&recent))
            .unwrap()
            .is_empty());
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
mod session_command_tests {
    use crate::cli::{created_session_line, Args, Command, SessionCmd};
    use clap::Parser;
    use wsx_core::runtime::SessionId;

    #[test]
    fn create_accepts_direct_scope_command_and_json_output() {
        let args = Args::try_parse_from([
            "wsx",
            "session",
            "create",
            "--project",
            "api",
            "--worktree",
            "fix/timeout",
            "--name",
            "review",
            "--command",
            "pi -p 'Review timeout handling'",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Session {
                subcommand: SessionCmd::Create {
                    name: Some(name),
                    command: Some(command),
                    json: true,
                    scope,
                }
            }) if name == "review"
                && command == "pi -p 'Review timeout handling'"
                && scope.project.as_deref() == Some("api")
                && scope.worktree.as_deref() == Some("fix/timeout")
        ));
    }

    #[test]
    fn restart_requires_a_selector_and_accepts_scope_and_json() {
        let args = Args::try_parse_from([
            "wsx",
            "session",
            "restart",
            "worker",
            "-p",
            "api",
            "-w",
            "fix/timeout",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Session {
                subcommand: SessionCmd::Restart {
                    session,
                    json: true,
                    scope,
                }
            }) if session == "worker"
                && scope.project.as_deref() == Some("api")
                && scope.worktree.as_deref() == Some("fix/timeout")
        ));
        assert!(Args::try_parse_from(["wsx", "session", "restart"]).is_err());
        assert!(Args::try_parse_from(["wsx", "session", "restart", "worker", "--nope"]).is_err());
    }

    #[test]
    fn created_identity_is_labeled_as_a_session() {
        assert_eq!(created_session_line(SessionId(42)), "session:  42");
    }

    #[test]
    fn delete_and_prompt_accept_the_same_target_scope() {
        for argv in [
            vec![
                "wsx",
                "session",
                "delete",
                "worker",
                "-p",
                "api",
                "-w",
                "fix/timeout",
                "--json",
            ],
            vec![
                "wsx",
                "session",
                "prompt",
                "worker",
                "Run tests",
                "-p",
                "api",
                "-w",
                "fix/timeout",
            ],
        ] {
            let args = Args::try_parse_from(argv).unwrap();
            match args.command.unwrap() {
                Command::Session {
                    subcommand: SessionCmd::Delete { scope, .. },
                }
                | Command::Session {
                    subcommand: SessionCmd::Prompt { scope, .. },
                } => {
                    assert_eq!(scope.project.as_deref(), Some("api"));
                    assert_eq!(scope.worktree.as_deref(), Some("fix/timeout"));
                }
                _ => panic!("expected scoped session command"),
            }
        }
    }
}

#[cfg(test)]
mod agent_command_tests {
    use crate::cli::{AgentCmd, Args, Command};
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
    fn exchange_commands_parse_exact_targets_bounds_and_writer_claims() {
        let request = Args::try_parse_from([
            "wsx",
            "agent",
            "request",
            "reviewer",
            "inspect lifecycle",
            "-p",
            "api",
            "-w",
            "fix/timeout",
            "--timeout",
            "300",
            "--writer",
            "--write-claim",
            "/repo/src",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            request.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Request {
                    session,
                    prompt,
                    timeout: 300,
                    writer: true,
                    write_claims,
                    stash_draft: false,
                    key: None,
                    json: true,
                    scope,
                }
            }) if session == "reviewer"
                && prompt.prompt.as_deref() == Some("inspect lifecycle")
                && write_claims == [std::path::PathBuf::from("/repo/src")]
                && scope.project.as_deref() == Some("api")
                && scope.worktree.as_deref() == Some("fix/timeout")
        ));
        assert!(Args::try_parse_from([
            "wsx",
            "agent",
            "request",
            "reviewer",
            "inspect",
            "--write-claim",
            "/repo/src"
        ])
        .is_err());

        for argv in [
            vec!["wsx", "agent", "inspect", "17", "--frame", "--json"],
            vec!["wsx", "agent", "wait", "17", "--timeout", "30", "--frame"],
            vec![
                "wsx",
                "agent",
                "continue",
                "17",
                "challenge evidence",
                "--timeout",
                "90",
            ],
            vec!["wsx", "agent", "cancel", "17", "--json"],
            vec!["wsx", "agent", "exchanges", "reviewer", "-p", "api"],
        ] {
            assert!(Args::try_parse_from(argv).is_ok());
        }
    }

    #[test]
    fn report_accepts_escape_interrupt_capability() {
        let args = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "claude",
            "--state",
            "working",
            "--escape-interrupts",
        ])
        .unwrap();

        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    escape_interrupts: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn claude_wake_commands_accept_prompt_bound_tokens() {
        let report = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "claude",
            "--state",
            "working",
            "--wake-token",
            "prompt-123",
        ])
        .unwrap();
        assert!(matches!(
            report.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    wake_token: Some(token),
                    ..
                }
            }) if token == "prompt-123"
        ));

        let heartbeat = Args::try_parse_from([
            "wsx",
            "agent",
            "wake-heartbeat",
            "7",
            "--prompt-id",
            "prompt-123",
        ])
        .unwrap();
        assert!(matches!(
            heartbeat.command,
            Some(Command::Agent {
                subcommand: AgentCmd::WakeHeartbeat { pane, prompt_id }
            }) if pane == "7" && prompt_id == "prompt-123"
        ));
    }

    #[test]
    fn detach_targets_only_the_current_managed_pane() {
        let args = Args::try_parse_from(["wsx", "agent", "detach"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Detach
            })
        ));
        assert!(Args::try_parse_from(["wsx", "agent", "detach", "7"]).is_err());
    }

    #[test]
    fn report_accepts_explicit_detach_and_defaults_to_attached() {
        for (extra, expected) in [(&[][..], false), (&["--detached"][..], true)] {
            let argv = [
                "wsx",
                "agent",
                "report",
                "7",
                "--provider",
                "pi",
                "--state",
                "idle",
            ]
            .into_iter()
            .chain(extra.iter().copied())
            .collect::<Vec<_>>();
            let args = Args::try_parse_from(argv).unwrap();
            assert!(matches!(
                args.command,
                Some(Command::Agent {
                    subcommand: AgentCmd::Report { detached, .. }
                }) if detached == expected
            ));
        }
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
    use crate::cli::{
        incompatible_runtime_status_json, runtime_lifecycle_lines, runtime_status_state, Args,
        Command, DaemonCmd,
    };
    use clap::Parser;
    use wsx_core::runtime::{self, DaemonLifecycle, DaemonPhase};

    #[test]
    fn daemon_stop_is_a_top_level_command() {
        let args = Args::try_parse_from(["wsx", "daemon", "stop"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Daemon {
                subcommand: DaemonCmd::Stop {
                    force: false,
                    json: false
                }
            })
        ));
    }

    #[test]
    fn daemon_recover_is_an_explicit_crash_budget_override() {
        let args = Args::try_parse_from(["wsx", "daemon", "recover"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Daemon {
                subcommand: DaemonCmd::Recover
            })
        ));
    }

    fn lifecycle(phase: DaemonPhase, daemon_revision: u32) -> DaemonLifecycle {
        DaemonLifecycle {
            protocol: runtime::PROTOCOL_VERSION,
            epoch: 1,
            binary_id: "binary".into(),
            version: runtime::WSX_VERSION.into(),
            daemon_revision,
            started_unix_ms: 1,
            phase,
            live_runtimes: 0,
            active_clients: 1,
            active_tuis: 0,
            recovered_from_backup: false,
            replacement_target: None,
            replacement_target_version: String::new(),
            replacement_blockers: Vec::new(),
            last_handoff_error: None,
        }
    }

    #[test]
    fn runtime_status_distinguishes_ready_upgrade_and_deferred_states() {
        assert_eq!(runtime_status_state(None), "ready");
        assert_eq!(
            runtime_status_state(Some(&lifecycle(
                DaemonPhase::Ready,
                runtime::DAEMON_REVISION
            ))),
            "ready"
        );
        assert_eq!(
            runtime_status_state(Some(&lifecycle(
                DaemonPhase::Ready,
                runtime::DAEMON_REVISION - 1
            ))),
            "upgrade_available"
        );
        assert_eq!(
            runtime_status_state(Some(&lifecycle(
                DaemonPhase::ReplacementPending,
                runtime::DAEMON_REVISION - 1
            ))),
            "replacement_deferred"
        );
    }

    #[test]
    fn incompatible_runtime_status_exposes_legacy_lifecycle_details() {
        let mut status = lifecycle(
            DaemonPhase::ReplacementPending,
            runtime::DAEMON_REVISION - 1,
        );
        status.live_runtimes = 3;
        status.active_clients = 2;
        status.active_tuis = 1;
        status.replacement_target = Some("0.28.1:1:2:3:4".into());
        status.replacement_target_version = "0.28.1".into();
        status.replacement_blockers = vec![runtime::ReplacementBlocker::WorkingAgent];

        let json = incompatible_runtime_status_json(
            std::path::Path::new("/test/wsx.sock"),
            Some(15),
            Some(&status),
            None,
        );
        assert_eq!(json["state"], "incompatible");
        assert_eq!(json["daemon_protocol"], 15);
        assert_eq!(json["lifecycle"]["phase"], "replacement_pending");
        assert_eq!(json["lifecycle"]["live_runtimes"], 3);
        assert_eq!(json["lifecycle"]["active_clients"], 2);
        assert_eq!(json["lifecycle"]["active_tuis"], 1);
        assert_eq!(json["lifecycle"]["replacement_target"], "0.28.1:1:2:3:4");
        assert_eq!(
            json["lifecycle"]["replacement_blockers"],
            serde_json::json!(["working_agent"])
        );

        let lines = runtime_lifecycle_lines(&status);
        assert!(lines.iter().any(|line| line.contains("3 live runtimes")));
        assert!(lines.iter().any(|line| line.contains("2 clients, 1 TUIs")));
        assert!(lines
            .iter()
            .any(|line| line == "Target: 0.28.1:1:2:3:4 (version 0.28.1)"));
        assert!(lines.iter().any(|line| line == "Blockers: WorkingAgent"));
    }
}

#[cfg(test)]
mod routine_client_wiring_tests {
    use crate::cli::Args;
    use clap::Parser;

    #[test]
    fn legacy_embedded_daemon_command_is_removed() {
        assert!(Args::try_parse_from(["wsx", "routine", "daemon", "start"]).is_err());
        assert!(Args::try_parse_from(["wsx", "routine-daemon-serve"]).is_err());
    }
}

#[cfg(test)]
mod session_resolution_tests {
    use crate::cli::{
        resolve_creation_worktree, resolve_session_in_snapshot, runtime_session_scope,
        RuntimeSessionScope, SessionScope,
    };
    use std::path::{Path, PathBuf};
    use wsx_core::{
        config::global::{GlobalConfig, ProjectEntry},
        model::workspace::{Project, WorkspaceState, WorktreeInfo},
        runtime::{
            Capabilities, Pane, PaneId, PaneLayout, Project as RuntimeProject, ProjectId, Session,
            SessionId, Snapshot, TerminalId, Worktree, WorktreeId,
        },
    };

    fn worktree(branch: &str, path: &str, alias: Option<&str>) -> WorktreeInfo {
        WorktreeInfo {
            name: branch.into(),
            branch: branch.into(),
            path: PathBuf::from(path),
            is_main: branch == "main",
            alias: alias.map(str::to_owned),
            sessions: Vec::new(),
            expanded: false,
            git_info: None,
            fetch_failed: false,
            fetch_fail_count: 0,
            fetch_fail_reason: None,
            last_fetched: None,
            git_info_fetched_at: None,
        }
    }

    fn project(name: &str, path: &str, worktrees: Vec<WorktreeInfo>) -> Project {
        Project {
            name: name.into(),
            path: PathBuf::from(path),
            default_branch: "main".into(),
            last_agent_active_unix_ms: None,
            last_terminal_active_unix_ms: None,
            worktrees,
            routines: Vec::new(),
            routine_revision: 0,
            routines_expanded: false,
            config: None,
            expanded: false,
            missing: false,
        }
    }

    #[test]
    fn creation_prefers_explicit_scope_then_cwd_then_only_worktree() {
        let workspace = WorkspaceState {
            projects: vec![
                project(
                    "api",
                    "/repos/api",
                    vec![worktree("main", "/repos/api", None)],
                ),
                project(
                    "web",
                    "/repos/web",
                    vec![
                        worktree("main", "/repos/web", None),
                        worktree("feature", "/repos/web-feature", Some("review")),
                    ],
                ),
            ],
        };
        let explicit = SessionScope {
            project: Some("web".into()),
            worktree: Some("review".into()),
        };
        let (resolved_project, resolved_worktree) =
            resolve_creation_worktree(&workspace, &explicit, Some(Path::new("/repos/api")))
                .unwrap();
        assert_eq!(resolved_project.name, "web");
        assert_eq!(resolved_worktree.branch, "feature");

        let inferred = SessionScope::default();
        let (resolved_project, resolved_worktree) = resolve_creation_worktree(
            &workspace,
            &inferred,
            Some(Path::new("/repos/web-feature/src")),
        )
        .unwrap();
        assert_eq!(resolved_project.name, "web");
        assert_eq!(resolved_worktree.branch, "feature");

        let only_project = WorkspaceState {
            projects: vec![project(
                "api",
                "/repos/api",
                vec![worktree("main", "/repos/api", None)],
            )],
        };
        let (_, resolved_worktree) =
            resolve_creation_worktree(&only_project, &inferred, None).unwrap();
        assert_eq!(resolved_worktree.branch, "main");
    }

    #[test]
    fn creation_rejects_ambiguous_project_and_worktree_targets() {
        let ambiguous_projects = WorkspaceState {
            projects: vec![
                project(
                    "api",
                    "/repos/api",
                    vec![worktree("main", "/repos/api", None)],
                ),
                project(
                    "web",
                    "/repos/web",
                    vec![worktree("main", "/repos/web", None)],
                ),
            ],
        };
        assert!(
            resolve_creation_worktree(&ambiguous_projects, &SessionScope::default(), None)
                .unwrap_err()
                .to_string()
                .contains("multiple projects")
        );

        let ambiguous_worktrees = WorkspaceState {
            projects: vec![project(
                "api",
                "/repos/api",
                vec![
                    worktree("main", "/repos/api", None),
                    worktree("feature", "/repos/api-feature", None),
                ],
            )],
        };
        assert!(
            resolve_creation_worktree(&ambiguous_worktrees, &SessionScope::default(), None)
                .unwrap_err()
                .to_string()
                .contains("multiple worktrees")
        );
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            protocol: 15,
            epoch: 1,
            revision: 1,
            projects: vec![
                RuntimeProject {
                    id: ProjectId(1),
                    path: "/repos/api".into(),
                    name: "api".into(),
                    revision: 1,
                    last_agent_active_unix_ms: None,
                    last_terminal_active_unix_ms: None,
                },
                RuntimeProject {
                    id: ProjectId(10),
                    path: "/repos/web".into(),
                    name: "web".into(),
                    revision: 1,
                    last_agent_active_unix_ms: None,
                    last_terminal_active_unix_ms: None,
                },
            ],
            worktrees: vec![
                Worktree {
                    id: WorktreeId(2),
                    project_id: ProjectId(1),
                    path: "/repos/api-feature".into(),
                    branch: "feature".into(),
                    revision: 1,
                },
                Worktree {
                    id: WorktreeId(11),
                    project_id: ProjectId(10),
                    path: "/repos/web-feature".into(),
                    branch: "feature".into(),
                    revision: 1,
                },
            ],
            sessions: vec![
                Session {
                    id: SessionId(3),
                    worktree_id: WorktreeId(2),
                    label: "worker".into(),
                    primary_pane: PaneId(4),
                    focused_pane: PaneId(4),
                    panes: vec![PaneId(4)],
                    layout: PaneLayout::Leaf { pane_id: PaneId(4) },
                    revision: 1,
                },
                Session {
                    id: SessionId(12),
                    worktree_id: WorktreeId(11),
                    label: "worker".into(),
                    primary_pane: PaneId(13),
                    focused_pane: PaneId(13),
                    panes: vec![PaneId(13)],
                    layout: PaneLayout::Leaf {
                        pane_id: PaneId(13),
                    },
                    revision: 1,
                },
            ],
            panes: vec![
                Pane {
                    id: PaneId(4),
                    terminal_id: TerminalId(5),
                    session_id: SessionId(3),
                    label: "terminal".into(),
                    agent: None,
                    exited: false,
                    revision: 1,
                },
                Pane {
                    id: PaneId(13),
                    terminal_id: TerminalId(14),
                    session_id: SessionId(12),
                    label: "terminal".into(),
                    agent: None,
                    exited: false,
                    revision: 1,
                },
            ],
            listening_ports: Vec::new(),
            pane_activity: Vec::new(),
            plugin_sidecars: Vec::new(),
            capabilities: Capabilities::default(),
        }
    }

    #[test]
    fn scoped_labels_resolve_once_and_exact_ids_reject_scope_mismatch() {
        let snapshot = snapshot();
        let mut config = GlobalConfig::default();
        config.projects.push(ProjectEntry {
            name: "api".into(),
            path: "/repos/api".into(),
            groups: Vec::new(),
            aliases: [("feature".into(), "review".into())].into(),
            worktree_order: Vec::new(),
        });
        let requested = SessionScope {
            project: Some("api".into()),
            worktree: Some("review".into()),
        };
        let scope = runtime_session_scope(&snapshot, &requested, Some(&config)).unwrap();
        let resolved = resolve_session_in_snapshot(&snapshot, "worker", scope).unwrap();
        assert_eq!(resolved.session_id, SessionId(3));
        assert_eq!(resolved.pane_id, PaneId(4));

        let unscoped = RuntimeSessionScope {
            project_id: None,
            worktree_id: None,
        };
        assert!(resolve_session_in_snapshot(&snapshot, "worker", unscoped)
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        assert!(resolve_session_in_snapshot(&snapshot, "12", scope)
            .unwrap_err()
            .to_string()
            .contains("outside the requested scope"));
    }

    #[test]
    fn exact_pane_id_preserves_the_selected_pane() {
        let snapshot = snapshot();
        let resolved = resolve_session_in_snapshot(
            &snapshot,
            "13",
            RuntimeSessionScope {
                project_id: None,
                worktree_id: None,
            },
        )
        .unwrap();
        assert_eq!(resolved.session_id, SessionId(12));
        assert_eq!(resolved.pane_id, PaneId(13));
        assert_eq!(resolved.label, "worker");
    }
}
