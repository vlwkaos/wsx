use super::*;

// --- Helpers ---

pub(crate) fn load_config() -> Result<GlobalConfig> {
    let (config, warning) = GlobalConfig::load()?;
    if let Some(w) = warning {
        eprintln!("warning: {}", w);
    }
    Ok(config)
}

pub(crate) fn load_full_workspace() -> Result<(GlobalConfig, WorkspaceState)> {
    let config = load_config()?;
    let workspace = ops::load_full_workspace(&runtime::Client::local(), &config)?;
    Ok((config, workspace))
}

pub(super) fn parse_group_key(value: &str) -> std::result::Result<GroupKey, String> {
    if value.eq_ignore_ascii_case("ungrouped") {
        Ok(GroupKey::Ungrouped)
    } else {
        GroupKey::named(value)
    }
}

/// Resolve projects by project name or one active group. `-p` takes precedence over `--group`.
// ^ [[wsx Architecture]] Workspace filtering has one optional group identity.
pub(super) fn resolve_projects<'a>(
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
    Ok(workspace
        .projects
        .iter()
        .filter(|project| project_matches_group(config.project_groups(&project.path), group))
        .collect())
}

pub(crate) fn resolve_project<'a>(
    workspace: &'a WorkspaceState,
    name: Option<&str>,
) -> Result<&'a Project> {
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

pub(super) fn cwd_worktree<'a>(
    projects: impl Iterator<Item = &'a Project>,
    cwd: Option<&Path>,
) -> Option<(&'a Project, &'a wsx_core::model::workspace::WorktreeInfo)> {
    let cwd = cwd?;
    projects
        .flat_map(|project| {
            project
                .worktrees
                .iter()
                .map(move |worktree| (project, worktree))
        })
        .filter(|(_, worktree)| cwd == worktree.path || cwd.starts_with(&worktree.path))
        .max_by_key(|(_, worktree)| worktree.path.components().count())
}

pub(super) fn resolve_creation_worktree<'a>(
    workspace: &'a WorkspaceState,
    scope: &SessionScope,
    cwd: Option<&Path>,
) -> Result<(&'a Project, &'a wsx_core::model::workspace::WorktreeInfo)> {
    let project = if let Some(name) = scope.project.as_deref() {
        resolve_project(workspace, Some(name))?
    } else if let Some((project, _)) = cwd_worktree(workspace.projects.iter(), cwd) {
        project
    } else {
        resolve_project(workspace, None)?
    };

    if let Some(selector) = scope.worktree.as_deref() {
        let matches = project
            .worktrees
            .iter()
            .filter(|worktree| {
                worktree.branch == selector
                    || worktree.alias.as_deref() == Some(selector)
                    || worktree.path == Path::new(selector)
            })
            .collect::<Vec<_>>();
        return match matches.as_slice() {
            [worktree] => Ok((project, *worktree)),
            [] => bail!(
                "worktree '{}' not found in project '{}'",
                selector,
                project.name
            ),
            _ => bail!(
                "worktree '{}' is ambiguous in project '{}'",
                selector,
                project.name
            ),
        };
    }

    if let Some((_, worktree)) = cwd_worktree(std::iter::once(project), cwd) {
        return Ok((project, worktree));
    }
    match project.worktrees.as_slice() {
        [worktree] => Ok((project, worktree)),
        [] => bail!("project '{}' has no worktrees", project.name),
        _ => bail!(
            "multiple worktrees in project '{}' — use -w to specify: {}",
            project.name,
            project
                .worktrees
                .iter()
                .map(|worktree| worktree.display_name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(super) fn activity_label(s: &wsx_core::model::workspace::SessionInfo) -> &'static str {
    session_state::status_label(s)
}

pub(super) fn git_label(wt: &wsx_core::model::workspace::WorktreeInfo) -> String {
    wt.git_info
        .as_ref()
        .map(|g| format!("+{}-{}", g.ahead, g.behind))
        .unwrap_or_else(|| "-".to_string())
}

pub(super) fn sessions_inline(wt: &wsx_core::model::workspace::WorktreeInfo) -> String {
    wt.sessions
        .iter()
        .map(|s| format!("{}[{}]", s.pane_id, activity_label(s)))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn print_table(headers: &[&str], rows: &[Vec<String>]) {
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

pub(super) fn cmd_status(json: bool, format: Format, group: Option<&GroupKey>) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, None, group)?;
    if json {
        // ^ Status JSON is a top-level project array; runtime status uses an object.
        print_json(&manager_status(&projects)?)?;
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

/// The status projects with what a manager needs for its next step, as added keys:
/// `work` on a worktree and `exchange` on a session.
/// ^ docs/agent-orchestration.md "One status call": one exchange per session and counts
/// for a worktree keep the output bounded.
pub(super) fn manager_status(projects: &[&Project]) -> Result<serde_json::Value> {
    let exchanges = list_agent_exchanges(None, RuntimeSessionScope::default())?;
    let mut output = serde_json::to_value(projects)?;
    for (project, project_row) in projects
        .iter()
        .zip(output.as_array_mut().into_iter().flatten())
    {
        // Git runs once per worktree; a few at a time keeps a large workspace quick.
        let mut work = Vec::new();
        for worktrees in project.worktrees.chunks(8) {
            std::thread::scope(|scope| {
                let jobs = worktrees
                    .iter()
                    .map(|worktree| {
                        scope.spawn(|| {
                            wsx_core::git::info::work_summary(
                                &worktree.path,
                                &project.default_branch,
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                work.extend(jobs.into_iter().map(|job| job.join().ok()));
            });
        }
        let worktree_rows = project_row["worktrees"]
            .as_array_mut()
            .into_iter()
            .flatten();
        for ((worktree, worktree_row), work) in
            project.worktrees.iter().zip(worktree_rows).zip(work)
        {
            worktree_row["work"] = serde_json::to_value(work)?;
            let session_rows = worktree_row["sessions"]
                .as_array_mut()
                .into_iter()
                .flatten();
            for (session, session_row) in worktree.sessions.iter().zip(session_rows) {
                // The active exchange of the session, or else the one that changed last.
                let exchange = exchanges
                    .iter()
                    .filter(|exchange| exchange.session_id == session.session_id)
                    .max_by_key(|exchange| (!exchange.is_terminal(), exchange.updated_unix_ms));
                session_row["exchange"] = serde_json::json!(exchange.map(|exchange| {
                    serde_json::json!({
                        "id": exchange.id,
                        "pane_id": exchange.pane_id,
                        "state": exchange.state,
                        "evidence": exchange.evidence,
                        "round": exchange.round,
                        "access": exchange.access,
                        "key": exchange.key,
                        "updated_unix_ms": exchange.updated_unix_ms,
                        "deadline_unix_ms": exchange.deadline_unix_ms,
                    })
                }));
            }
        }
    }
    Ok(output)
}

pub(super) fn cmd_worktree_create(
    branch: &str,
    project_name: Option<&str>,
    from: Option<&str>,
    json: bool,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project_name)?;
    refuse_start_point_of_existing_branch(project, branch, from)?;
    let proj_config = project.config.clone().unwrap_or_default();
    let created = ops::create_configured_worktree(
        &runtime::Client::local(),
        &config,
        &project.name,
        &project.path,
        from.unwrap_or(&project.default_branch),
        &proj_config,
        branch,
        wsx_core::model::workspace::WorktreeInitialSession::Shell,
    )?;
    if let Some(warning) = created.warning {
        eprintln!("warning: {warning}");
    }
    let session_id = created.session.as_ref().map(|(session_id, _)| *session_id);
    if json {
        // ^ A lookup failure must not hide the worktree that already exists.
        let pane_id = session_id.and_then(|session_id| created_session_pane(session_id).0);
        print_json(&serde_json::json!({
            "worktree": created.path,
            "branch": branch,
            "project": project.name,
            "session_id": session_id,
            "pane_id": pane_id,
        }))?;
    } else {
        println!("worktree: {}", created.path.display());
        if let Some(session_id) = session_id {
            println!("{}", created_session_line(session_id));
        }
    }
    if let Some(error) = created.session_error {
        anyhow::bail!("worktree created, but its initial wsx session failed: {error}");
    }
    Ok(())
}

// ^ An existing branch is checked out as it is, so a start point cannot apply.
pub(super) fn refuse_start_point_of_existing_branch(
    project: &Project,
    branch: &str,
    from: Option<&str>,
) -> Result<()> {
    if from.is_some() && wsx_core::git::worktree::branch_exists(&project.path, branch) {
        bail!("branch '{branch}' already exists; omit --from to check it out");
    }
    Ok(())
}

pub(super) fn created_session_line(session_id: runtime::SessionId) -> String {
    format!("session:  {session_id}")
}

/// The pane of a session that was just created, and its agent when one already reported.
/// ^ Best effort: the session exists even when this second read fails.
pub(super) fn created_session_pane(
    session_id: runtime::SessionId,
) -> (Option<runtime::PaneId>, Option<String>) {
    let Ok(snapshot) = ops::runtime_snapshot(&runtime::Client::local()) else {
        return (None, None);
    };
    let pane_id = snapshot
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .map(|session| session.focused_pane);
    let agent = snapshot
        .panes
        .iter()
        .find(|pane| Some(pane.id) == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .filter(|agent| agent.attached)
        .map(|agent| agent.provider.clone());
    (pane_id, agent)
}

pub(super) fn cmd_worktree_delete(branch: &str, project_name: Option<&str>) -> Result<()> {
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
    let branch_deleted = ops::delete_worktree(
        &runtime::Client::local(),
        &project.path,
        &wt.path,
        &wt.branch,
    )?;
    println!("deleted worktree: {}", wt.path.display());
    if !branch_deleted {
        println!(
            "kept branch {}: it has commits that are not merged",
            wt.branch
        );
    }
    Ok(())
}

pub(super) fn cmd_worktree_list(
    project_name: Option<&str>,
    json: bool,
    format: Format,
    group: Option<&GroupKey>,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, project_name, group)?;
    if json {
        let worktrees: Vec<_> = projects.iter().flat_map(|p| p.worktrees.iter()).collect();
        print_json(&worktrees)?;
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
