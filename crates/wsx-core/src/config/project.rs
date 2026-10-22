// Canonical per-project configuration and legacy .gtrconfig migration.
// ref: README.md#project-configuration

use crate::model::workspace::{DefaultSessionConfig, ProjectConfig, TaskConfig, TaskSandbox};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_MIN_FREE_GB: u64 = 100_000;

/// `tasks.preview` checked: a command, a path that answers 200 and a port range above 1023.
fn preview_config(file: TaskPreviewFile) -> Result<crate::model::workspace::TaskPreview, String> {
    let command = file.command.trim().to_string();
    if command.is_empty() || command.len() > MAX_DEFAULT_COMMAND_BYTES {
        return Err(format!(
            "tasks.preview.command must be 1..={MAX_DEFAULT_COMMAND_BYTES} bytes"
        ));
    }
    let ready = file.ready.unwrap_or_else(|| "/".into());
    if !ready.starts_with('/')
        || ready.len() > 1024
        || ready.chars().any(|c| c.is_control() || c == ' ')
    {
        return Err("tasks.preview.ready must be a path such as /health".into());
    }
    let ports = file.ports.unwrap_or_else(|| "9100-9199".into());
    let range = ports
        .split_once('-')
        .and_then(|(first, last)| {
            Some((
                first.trim().parse::<u16>().ok()?,
                last.trim().parse::<u16>().ok()?,
            ))
        })
        .filter(|(first, last)| *first > 1023 && first <= last)
        .ok_or_else(|| {
            format!("tasks.preview.ports must be <first>-<last> above 1023, not {ports}")
        })?;
    Ok(crate::model::workspace::TaskPreview {
        command,
        ready,
        ports: range,
    })
}
const PROJECT_CONFIG_FILE: &str = "wsx.config.yml";
const LEGACY_CONFIG_FILE: &str = ".gtrconfig";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_BRANCH_PREFIX_BYTES: usize = 256;
const MAX_DEFAULT_COMMAND_BYTES: usize = 16 * 1024;
const MAX_TASK_WORD_BYTES: usize = 256;
/// `tasks.manager.name`: one short line the owner reads as the manager's name.
const MAX_MANAGER_NAME_BYTES: usize = 64;
const MAX_TASK_LIST_ITEMS: usize = 64;
const MAX_CHECK_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const MIN_ROTATE_TOKENS: u64 = 1_000;
const MAX_ROTATE_TOKENS: u64 = 2_000_000;
const TASK_ADAPTERS: &[&str] = &["claude"];
const BARE_FILE_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];
const PROJECT_CONFIG_TEMPLATE: &str = "hooks:\n  postCreate:\ncopy:\n  include: []\n  exclude: []\ngit:\n  subtrees: []\nworktree:\n  branchPrefix:\n  # defaultSession:\n  #   enabled: true\n  #   command:\n";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
enum PublishMode {
    Create,
    Replace,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectConfigFile {
    hooks: HookConfig,
    copy: CopyConfig,
    git: GitConfig,
    #[serde(skip_serializing_if = "WorktreeConfig::is_empty")]
    worktree: WorktreeConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    tasks: Option<TasksFile>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TasksFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<TaskAgentFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    checks: Vec<String>,
    #[serde(
        rename = "checkTimeoutSeconds",
        skip_serializing_if = "Option::is_none"
    )]
    check_timeout_seconds: Option<u64>,
    #[serde(rename = "minFreeGb", skip_serializing_if = "Option::is_none")]
    min_free_gb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    land: Option<TaskLandFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deploy: Option<TaskCommandFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release: Option<TaskCommandFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manager: Option<TaskManagerFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox: Option<TaskSandboxFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<TaskPreviewFile>,
    /// `owner` or `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    skills: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<TaskWorkflowFile>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TaskWorkflowFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    batch: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TaskPreviewFile {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ready: Option<String>,
    /// `<first>-<last>`, such as `9100-9199`.
    #[serde(skip_serializing_if = "Option::is_none")]
    ports: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TaskSandboxFile {
    #[serde(rename = "allowedDomains", skip_serializing_if = "Vec::is_empty")]
    allowed_domains: Vec<String>,
    #[serde(rename = "allowWrite", skip_serializing_if = "Vec::is_empty")]
    allow_write: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TaskManagerFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adapter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(rename = "eventModel", skip_serializing_if = "Option::is_none")]
    event_model: Option<String>,
    #[serde(rename = "rotateTokens", skip_serializing_if = "Option::is_none")]
    rotate_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TaskCommandFile {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    approve: Option<bool>,
    /// Deploy only: the deployed site is public, so open "To check" items stop it.
    #[serde(skip_serializing_if = "Option::is_none")]
    public: Option<bool>,
    /// Deploy only: run after each land of the default branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    auto: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TaskLandFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    approve: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TaskAgentFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    adapter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct HookConfig {
    #[serde(rename = "postCreate", skip_serializing_if = "Option::is_none")]
    post_create: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct CopyConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct GitConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    subtrees: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct WorktreeConfig {
    #[serde(rename = "branchPrefix", skip_serializing_if = "Option::is_none")]
    branch_prefix: Option<String>,
    #[serde(rename = "defaultSession", skip_serializing_if = "Option::is_none")]
    default_session: Option<DefaultSessionFile>,
}

impl WorktreeConfig {
    fn is_empty(&self) -> bool {
        self.branch_prefix.is_none() && self.default_session.is_none()
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct DefaultSessionFile {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

pub fn load_project_config(repo_path: &Path) -> ProjectConfig {
    let canonical = repo_path.join(PROJECT_CONFIG_FILE);
    if canonical.exists() {
        return load_yaml(&canonical);
    }

    let legacy = repo_path.join(LEGACY_CONFIG_FILE);
    if !legacy.exists() {
        return ProjectConfig::default();
    }
    if let Err(error) = read_bounded(&legacy) {
        return ProjectConfig {
            notice: Some(format!("Could not read {}: {error}", legacy.display())),
            ..ProjectConfig::default()
        };
    }

    let mut config = load_legacy(&legacy);
    config.notice = Some(match write_yaml(&canonical, &config) {
        Ok(()) => format!(
            "Created {PROJECT_CONFIG_FILE} from {LEGACY_CONFIG_FILE}; remove the legacy file when ready"
        ),
        Err(error) => format!(
            "Using {LEGACY_CONFIG_FILE}; could not create {PROJECT_CONFIG_FILE}: {error}"
        ),
    });
    config
}

/// Creates a useful canonical template only when editing begins. Existing
/// nonempty regular files are never replaced, including malformed files.
pub fn prepare_project_config_for_edit(repo_path: &Path) -> Result<PathBuf, String> {
    let canonical = repo_path.join(PROJECT_CONFIG_FILE);
    match fs::symlink_metadata(&canonical) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(format!("{} is not a regular file", canonical.display()));
            }
            let text = read_bounded(&canonical)?;
            if text.trim().is_empty() {
                write_text_atomic(&canonical, PROJECT_CONFIG_TEMPLATE, PublishMode::Replace)?;
            }
            return Ok(canonical);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }

    let legacy = repo_path.join(LEGACY_CONFIG_FILE);
    if legacy.exists() {
        let migrated = load_project_config(repo_path);
        if canonical.exists() {
            return Ok(canonical);
        }
        return Err(migrated.notice.unwrap_or_else(|| {
            format!("could not create {PROJECT_CONFIG_FILE} from {LEGACY_CONFIG_FILE}")
        }));
    }

    write_text_atomic(&canonical, PROJECT_CONFIG_TEMPLATE, PublishMode::Create)?;
    Ok(canonical)
}

fn load_yaml(path: &Path) -> ProjectConfig {
    let text = match read_bounded(path) {
        Ok(text) => text,
        Err(error) => {
            return ProjectConfig {
                notice: Some(format!("Could not read {}: {error}", path.display())),
                ..ProjectConfig::default()
            };
        }
    };
    parse_yaml(&text).unwrap_or_else(|error| ProjectConfig {
        notice: Some(format!(
            "Could not parse {} (using defaults): {error}",
            path.display()
        )),
        ..ProjectConfig::default()
    })
}

fn parse_yaml(text: &str) -> Result<ProjectConfig, String> {
    let file =
        yaml_serde::from_str::<ProjectConfigFile>(text).map_err(|error| error.to_string())?;
    validate_project_file(file)
}

/// The task settings of a project. Unlike `load_project_config`, a file that
/// cannot be read or parsed is an error: a task must not run with default tools.
pub fn load_task_config(repo_path: &Path) -> Result<TaskConfig, String> {
    let canonical = repo_path.join(PROJECT_CONFIG_FILE);
    if !canonical.exists() {
        return Ok(TaskConfig::default());
    }
    let text = read_bounded(&canonical)
        .map_err(|error| format!("cannot read {}: {error}", canonical.display()))?;
    parse_yaml(&text)
        .map(|config| config.tasks)
        .map_err(|error| format!("cannot parse {}: {error}", canonical.display()))
}

fn validate_tasks(file: Option<TasksFile>) -> Result<TaskConfig, String> {
    let mut config = TaskConfig::default();
    let Some(file) = file else {
        return Ok(config);
    };
    if let Some(agent) = file.agent {
        if let Some(adapter) = agent.adapter {
            if !TASK_ADAPTERS.contains(&adapter.as_str()) {
                return Err(format!(
                    "tasks.agent.adapter must be one of: {}",
                    TASK_ADAPTERS.join(", ")
                ));
            }
            config.adapter = adapter;
        }
        if let Some(model) =
            validate_optional_text(agent.model, "tasks.agent.model", MAX_TASK_WORD_BYTES)?
        {
            if model.contains(char::is_whitespace) {
                return Err("tasks.agent.model must be one word".into());
            }
            config.model = model;
        }
        if let Some(tools) = agent.tools {
            config.tools = validate_tools(tools, "tasks.agent.tools")?;
        }
    }
    if let Some(sandbox) = file.sandbox {
        config.sandbox = validate_sandbox(sandbox)?;
    }
    config.owner_skills = match file.skills.as_deref() {
        None | Some("none") => false,
        Some("owner") => true,
        Some(other) => return Err(format!("tasks.skills must be owner or none, not {other:?}")),
    };
    if let Some(workflow) = file.workflow {
        let text = |value: Option<String>, field: &str, default: &mut String| {
            let Some(value) = value else { return Ok(()) };
            if value.len() > MAX_DEFAULT_COMMAND_BYTES
                || value.chars().any(|c| c.is_control() && c != '\n')
            {
                return Err(format!(
                    "tasks.workflow.{field} must be at most {MAX_DEFAULT_COMMAND_BYTES} bytes of text"
                ));
            }
            *default = value.trim().to_string();
            Ok(())
        };
        text(workflow.build, "build", &mut config.workflow.build)?;
        text(workflow.review, "review", &mut config.workflow.review)?;
        text(workflow.batch, "batch", &mut config.workflow.batch)?;
    }
    config.checks = validate_list(file.checks, "tasks.checks", MAX_DEFAULT_COMMAND_BYTES)?;
    if let Some(preview) = file.preview {
        config.preview = Some(preview_config(preview)?);
    }
    if let Some(gb) = file.min_free_gb {
        if gb > MAX_MIN_FREE_GB {
            return Err(format!("tasks.minFreeGb must be 0..={MAX_MIN_FREE_GB}"));
        }
        config.min_free_gb = gb;
    }
    if let Some(seconds) = file.check_timeout_seconds {
        if !(1..=MAX_CHECK_TIMEOUT_SECS).contains(&seconds) {
            return Err(format!(
                "tasks.checkTimeoutSeconds must be 1..={MAX_CHECK_TIMEOUT_SECS}"
            ));
        }
        config.check_timeout_secs = seconds;
    }
    if let Some(approve) = file.deploy.as_ref().and_then(|deploy| deploy.approve) {
        config.deploy_approve = approve;
    }
    if let Some(public) = file.deploy.as_ref().and_then(|deploy| deploy.public) {
        config.deploy_public = public;
    }
    if let Some(auto) = file.deploy.as_ref().and_then(|deploy| deploy.auto) {
        config.deploy_auto = auto;
    }
    if file
        .release
        .as_ref()
        .is_some_and(|release| release.auto.is_some())
    {
        return Err(
            "tasks.release.auto is not a setting: a release is the owner's decision".into(),
        );
    }
    if let Some(approve) = file.release.as_ref().and_then(|release| release.approve) {
        config.release_approve = approve;
    }
    if file
        .release
        .as_ref()
        .is_some_and(|release| release.public.is_some())
    {
        return Err("tasks.release.public is not a setting: a release is always public".into());
    }
    config.deploy_command = file
        .deploy
        .map(|deploy| {
            validate_optional_text(
                Some(deploy.command),
                "tasks.deploy.command",
                MAX_DEFAULT_COMMAND_BYTES,
            )
        })
        .transpose()?
        .flatten();
    config.release_command = file
        .release
        .map(|release| {
            validate_optional_text(
                Some(release.command),
                "tasks.release.command",
                MAX_DEFAULT_COMMAND_BYTES,
            )
        })
        .transpose()?
        .flatten();
    if let Some(manager) = file.manager {
        // ^ Trimmed first: an empty name is no name, and the default "Manager" shows.
        config.manager_name = validate_optional_text(
            manager.name.map(|name| name.trim().to_string()),
            "tasks.manager.name",
            MAX_MANAGER_NAME_BYTES,
        )?;
        if let Some(adapter) = manager.adapter {
            if !TASK_ADAPTERS.contains(&adapter.as_str()) {
                return Err(format!(
                    "tasks.manager.adapter must be one of: {}",
                    TASK_ADAPTERS.join(", ")
                ));
            }
        }
        if let Some(model) =
            validate_optional_text(manager.model, "tasks.manager.model", MAX_TASK_WORD_BYTES)?
        {
            if model.contains(char::is_whitespace) {
                return Err("tasks.manager.model must be one word".into());
            }
            config.manager_model = model;
        }
        if let Some(model) = validate_optional_text(
            manager.event_model,
            "tasks.manager.eventModel",
            MAX_TASK_WORD_BYTES,
        )? {
            if model.contains(char::is_whitespace) {
                return Err("tasks.manager.eventModel must be one word".into());
            }
            config.manager_event_model = (model != "off").then_some(model);
        }
        if let Some(tokens) = manager.rotate_tokens {
            if !(MIN_ROTATE_TOKENS..=MAX_ROTATE_TOKENS).contains(&tokens) {
                return Err(format!(
                    "tasks.manager.rotateTokens must be {MIN_ROTATE_TOKENS}..={MAX_ROTATE_TOKENS}"
                ));
            }
            config.manager_rotate_tokens = tokens;
        }
        if let Some(tools) = manager.tools {
            config.manager_tools = validate_tools(tools, "tasks.manager.tools")?;
        }
    }
    if let Some(land) = file.land {
        if let Some(approve) = land.approve {
            config.land_approve = approve;
        }
        if let Some(auto) = land.auto {
            config.land_auto = auto;
        }
    }
    Ok(config)
}

/// Extra tool rules for an agent, each one `tool_refusal` accepts.
fn validate_tools(tools: Vec<String>, field: &str) -> Result<Vec<String>, String> {
    let tools = validate_list(tools, field, MAX_TASK_WORD_BYTES)?;
    if let Some((rule, reason)) = tools
        .iter()
        .find_map(|tool| tool_refusal(tool).map(|reason| (tool, reason)))
    {
        return Err(format!("{field} has {rule}: {reason}; remove it"));
    }
    Ok(tools)
}

/// `Bash` or `Bash(...)`: the worker always has Bash, inside the sandbox.
pub fn is_bash_rule(tool: &str) -> bool {
    tool == "Bash" || tool.starts_with("Bash(")
}

/// Why a tool rule cannot be given to an agent, if it cannot. ref: docs/tasks.md "Permissions"
pub fn tool_refusal(tool: &str) -> Option<&'static str> {
    if is_bash_rule(tool) {
        Some("the sandbox replaces Bash rules; allow a host or a path instead")
    } else if BARE_FILE_TOOLS.contains(&tool) {
        // ^ A bare file-tool rule would allow every path and skip the auto-mode classifier.
        Some(
            "file tools are always on in the working copy and reviewed by the classifier elsewhere",
        )
    } else {
        None
    }
}

/// A host for the sandbox's allow list: a name with an optional `*.` in front, no scheme,
/// path or port.
pub fn validate_host(host: &str) -> Result<(), String> {
    let name = host.strip_prefix("*.").unwrap_or(host);
    let valid = !name.is_empty()
        && host.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{host:?} is not a host name such as crates.io or *.npmjs.org"
        ))
    }
}

/// A path for the sandbox's write list: absolute or under `~/`, without `..`, and never
/// the home folder or the root itself.
pub fn validate_write_path(path: &str) -> Result<(), String> {
    let rest = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix('/'))
        .ok_or_else(|| format!("{path:?} must be absolute or start with ~/"))?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty()
        || rest
            .split('/')
            .any(|part| part.is_empty() || part == ".." || part == ".")
        || path.chars().any(char::is_control)
        || path.len() > MAX_TASK_WORD_BYTES
    {
        return Err(format!(
            "{path:?} must name a folder below / or ~/ without . or .. parts"
        ));
    }
    Ok(())
}

fn validate_sandbox(file: TaskSandboxFile) -> Result<TaskSandbox, String> {
    let allowed_domains = validate_list(
        file.allowed_domains,
        "tasks.sandbox.allowedDomains",
        MAX_TASK_WORD_BYTES,
    )?;
    for host in &allowed_domains {
        validate_host(host).map_err(|error| format!("tasks.sandbox.allowedDomains: {error}"))?;
    }
    let allow_write = validate_list(
        file.allow_write,
        "tasks.sandbox.allowWrite",
        MAX_TASK_WORD_BYTES,
    )?;
    for path in &allow_write {
        validate_write_path(path).map_err(|error| format!("tasks.sandbox.allowWrite: {error}"))?;
    }
    Ok(TaskSandbox {
        allowed_domains,
        allow_write,
    })
}

fn validate_list(
    values: Vec<String>,
    field: &str,
    max_bytes: usize,
) -> Result<Vec<String>, String> {
    if values.len() > MAX_TASK_LIST_ITEMS {
        return Err(format!(
            "{field} must have at most {MAX_TASK_LIST_ITEMS} items"
        ));
    }
    values
        .into_iter()
        .map(|value| {
            validate_optional_text(Some(value), field, max_bytes)?
                .ok_or_else(|| format!("{field} must not contain an empty item"))
        })
        .collect()
}

fn validate_project_file(file: ProjectConfigFile) -> Result<ProjectConfig, String> {
    let git_subtrees = validate_subtree_paths(file.git.subtrees)?;
    let branch_prefix = validate_optional_text(
        file.worktree.branch_prefix,
        "worktree.branchPrefix",
        MAX_BRANCH_PREFIX_BYTES,
    )?
    .unwrap_or_default();
    let default_session = file
        .worktree
        .default_session
        .map(|session| {
            Ok::<DefaultSessionConfig, String>(DefaultSessionConfig {
                enabled: session.enabled,
                command: validate_optional_text(
                    session.command,
                    "worktree.defaultSession.command",
                    MAX_DEFAULT_COMMAND_BYTES,
                )?,
            })
        })
        .transpose()?;
    let tasks = validate_tasks(file.tasks)?;
    Ok(ProjectConfig {
        tasks,
        post_create: file.hooks.post_create,
        copy_includes: file.copy.include,
        copy_excludes: file.copy.exclude,
        git_subtrees,
        branch_prefix,
        default_session,
        notice: None,
    })
}

fn validate_optional_text(
    value: Option<String>,
    field: &str,
    max_bytes: usize,
) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.len() > max_bytes {
        return Err(format!("{field} must be at most {max_bytes} bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{field} must not contain control characters"));
    }
    if value.trim() != value {
        return Err(format!("{field} must not start or end with whitespace"));
    }
    Ok((!value.is_empty()).then_some(value))
}

fn validate_subtree_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    let mut validated = Vec::with_capacity(paths.len());
    for path in paths {
        let text = path.to_string_lossy();
        let valid = !text.is_empty()
            && !text.chars().any(char::is_control)
            && !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)));
        if !valid {
            return Err(format!(
                "git.subtrees path must be a normalized relative path: {}",
                path.display()
            ));
        }
        if !validated.contains(&path) {
            validated.push(path);
        }
    }
    Ok(validated)
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "file is {} bytes; maximum is {MAX_CONFIG_BYTES}",
            metadata.len()
        ));
    }
    fs::read_to_string(path).map_err(|error| error.to_string())
}

fn write_yaml(path: &Path, config: &ProjectConfig) -> Result<(), String> {
    let document = ProjectConfigFile {
        hooks: HookConfig {
            post_create: config.post_create.clone(),
        },
        copy: CopyConfig {
            include: config.copy_includes.clone(),
            exclude: config.copy_excludes.clone(),
        },
        git: GitConfig {
            subtrees: config.git_subtrees.clone(),
        },
        worktree: WorktreeConfig {
            branch_prefix: (!config.branch_prefix.is_empty()).then(|| config.branch_prefix.clone()),
            default_session: config
                .default_session
                .as_ref()
                .map(|session| DefaultSessionFile {
                    enabled: session.enabled,
                    command: session.command.clone(),
                }),
        },
        tasks: None,
    };
    let text = yaml_serde::to_string(&document).map_err(|error| error.to_string())?;
    write_text_atomic(path, &text, PublishMode::Create)
}

fn write_text_atomic(path: &Path, text: &str, mode: PublishMode) -> Result<(), String> {
    let tmp = temporary_path(path);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|error| error.to_string())?;
        file.write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        match mode {
            PublishMode::Replace => fs::rename(&tmp, path).map_err(|error| error.to_string()),
            PublishMode::Create => {
                fs::hard_link(&tmp, path).map_err(|error| error.to_string())?;
                let _ = fs::remove_file(&tmp);
                Ok(())
            }
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(name)
}

fn load_legacy(config_path: &Path) -> ProjectConfig {
    let path = config_path.to_string_lossy();
    ProjectConfig {
        post_create: git_config_get(&path, "hooks.postCreate"),
        copy_includes: git_config_get_all(&path, "copy.include"),
        copy_excludes: git_config_get_all(&path, "copy.exclude"),
        git_subtrees: Vec::new(),
        ..ProjectConfig::default()
    }
}

fn git_config_get(config_path: &str, key: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "-f", config_path, "--get", key])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_config_get_all(config_path: &str, key: &str) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .args(["config", "-f", config_path, "--get-all", key])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::current_dir()
                .unwrap()
                .join(".work/project-config-tests")
                .join(format!("{name}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loads_canonical_yaml() {
        let dir = TestDir::new("yaml");
        fs::write(
            dir.0.join(PROJECT_CONFIG_FILE),
            "hooks:\n  postCreate: cargo build\ncopy:\n  include: [.env]\n  exclude: [target]\ngit:\n  subtrees: [vendor/asched, vendor/herdr]\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert_eq!(config.post_create.as_deref(), Some("cargo build"));
        assert_eq!(config.copy_includes, [".env"]);
        assert_eq!(config.copy_excludes, ["target"]);
        assert_eq!(
            config.git_subtrees,
            [
                PathBuf::from("vendor/asched"),
                PathBuf::from("vendor/herdr")
            ]
        );
        assert!(config.notice.is_none());
    }

    #[test]
    fn loads_worktree_creation_defaults() {
        let dir = TestDir::new("worktree-defaults");
        fs::write(
            dir.0.join(PROJECT_CONFIG_FILE),
            "worktree:\n  branchPrefix: feature/\n  defaultSession:\n    enabled: true\n    command: cargo watch\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert_eq!(config.branch_prefix, "feature/");
        assert_eq!(
            config.default_session,
            Some(DefaultSessionConfig {
                enabled: true,
                command: Some("cargo watch".into()),
            })
        );
        assert!(config.notice.is_none());
    }

    #[test]
    fn missing_worktree_config_preserves_unspecified_session_behavior() {
        let dir = TestDir::new("worktree-missing");
        fs::write(dir.0.join(PROJECT_CONFIG_FILE), "hooks: {}\n").unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.branch_prefix.is_empty());
        assert!(config.default_session.is_none());
        assert!(config.notice.is_none());
    }

    #[test]
    fn control_characters_in_default_command_reject_the_entire_project_config() {
        let dir = TestDir::new("worktree-command-control");
        fs::write(
            dir.0.join(PROJECT_CONFIG_FILE),
            "worktree:\n  defaultSession:\n    enabled: true\n    command: \"touch one\\nsecond\"\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.default_session.is_none());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("must not contain control characters")));
    }

    #[test]
    fn oversized_branch_prefix_rejects_the_entire_project_config() {
        let dir = TestDir::new("worktree-prefix-oversized");
        let prefix = "x".repeat(MAX_BRANCH_PREFIX_BYTES + 1);
        fs::write(
            dir.0.join(PROJECT_CONFIG_FILE),
            format!("worktree:\n  branchPrefix: {prefix}\n"),
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.branch_prefix.is_empty());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("must be at most 256 bytes")));
    }

    #[test]
    fn invalid_worktree_text_rejects_the_entire_project_config() {
        let dir = TestDir::new("worktree-invalid");
        fs::write(
            dir.0.join(PROJECT_CONFIG_FILE),
            "worktree:\n  branchPrefix: \" feature/\"\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.branch_prefix.is_empty());
        assert!(config.default_session.is_none());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("must not start or end with whitespace")));
    }

    #[test]
    fn migrates_legacy_config_without_deleting_it() {
        let dir = TestDir::new("legacy");
        let legacy = dir.0.join(LEGACY_CONFIG_FILE);
        fs::write(
            &legacy,
            "[hooks]\n\tpostCreate = cargo build\n[copy]\n\tinclude = .env\n\tinclude = .tool-versions\n\texclude = target\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert_eq!(config.post_create.as_deref(), Some("cargo build"));
        assert_eq!(config.copy_includes, [".env", ".tool-versions"]);
        assert_eq!(config.copy_excludes, ["target"]);
        assert!(legacy.exists());
        assert!(dir.0.join(PROJECT_CONFIG_FILE).exists());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Created")));

        let canonical = load_project_config(&dir.0);
        assert_eq!(canonical.post_create, config.post_create);
        assert_eq!(canonical.copy_includes, config.copy_includes);
        assert_eq!(canonical.copy_excludes, config.copy_excludes);
        assert!(canonical.notice.is_none());
    }

    #[test]
    fn oversized_legacy_config_is_not_parsed_or_migrated() {
        let dir = TestDir::new("oversized-legacy");
        fs::File::create(dir.0.join(LEGACY_CONFIG_FILE))
            .unwrap()
            .set_len(MAX_CONFIG_BYTES + 1)
            .unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.post_create.is_none());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("maximum")));
        assert!(!dir.0.join(PROJECT_CONFIG_FILE).exists());
    }

    #[test]
    fn invalid_subtree_path_fails_the_strict_project_config() {
        let dir = TestDir::new("invalid-subtree");
        fs::write(
            dir.0.join(PROJECT_CONFIG_FILE),
            "git:\n  subtrees: [../outside]\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.git_subtrees.is_empty());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("normalized relative path")));
    }

    #[test]
    fn duplicate_subtree_paths_are_deduplicated_in_order() {
        let dir = TestDir::new("duplicate-subtree");
        fs::write(
            dir.0.join(PROJECT_CONFIG_FILE),
            "git:\n  subtrees: [vendor/asched, vendor/asched, vendor/herdr]\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert_eq!(
            config.git_subtrees,
            [
                PathBuf::from("vendor/asched"),
                PathBuf::from("vendor/herdr")
            ]
        );
    }

    #[test]
    fn malformed_canonical_config_does_not_fall_back_to_legacy() {
        let dir = TestDir::new("invalid");
        fs::write(dir.0.join(PROJECT_CONFIG_FILE), "hooks: [invalid]\n").unwrap();
        fs::write(
            dir.0.join(LEGACY_CONFIG_FILE),
            "[hooks]\n\tpostCreate = should-not-run\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.post_create.is_none());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Could not parse")));
    }

    #[test]
    fn edit_preparation_creates_a_valid_template_only_when_missing() {
        let dir = TestDir::new("edit-missing");

        let path = prepare_project_config_for_edit(&dir.0).unwrap();

        assert_eq!(path, dir.0.join(PROJECT_CONFIG_FILE));
        assert_eq!(fs::read_to_string(&path).unwrap(), PROJECT_CONFIG_TEMPLATE);
        let config = load_project_config(&dir.0);
        assert!(config.post_create.is_none());
        assert!(config.copy_includes.is_empty());
        assert!(config.copy_excludes.is_empty());
        assert!(config.notice.is_none());
    }

    #[test]
    fn edit_preparation_replaces_whitespace_only_config() {
        let dir = TestDir::new("edit-empty");
        let path = dir.0.join(PROJECT_CONFIG_FILE);
        fs::write(&path, "  \n\t").unwrap();

        prepare_project_config_for_edit(&dir.0).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), PROJECT_CONFIG_TEMPLATE);
    }

    #[test]
    fn edit_preparation_preserves_nonempty_malformed_config() {
        let dir = TestDir::new("edit-invalid");
        let path = dir.0.join(PROJECT_CONFIG_FILE);
        let malformed = "hooks: [invalid]\n";
        fs::write(&path, malformed).unwrap();

        prepare_project_config_for_edit(&dir.0).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), malformed);
    }

    #[test]
    fn edit_preparation_migrates_legacy_content_before_editing() {
        let dir = TestDir::new("edit-legacy");
        fs::write(
            dir.0.join(LEGACY_CONFIG_FILE),
            "[hooks]\n\tpostCreate = cargo check\n",
        )
        .unwrap();

        let path = prepare_project_config_for_edit(&dir.0).unwrap();
        let config = load_project_config(&dir.0);

        assert_eq!(path, dir.0.join(PROJECT_CONFIG_FILE));
        assert!(path.is_file());
        assert_eq!(config.post_create.as_deref(), Some("cargo check"));
        let serialized = fs::read_to_string(&path).unwrap();
        assert!(
            serialized.contains("postCreate: cargo check"),
            "{serialized}"
        );
    }

    #[test]
    fn edit_preparation_rejects_nonregular_canonical_path() {
        let dir = TestDir::new("edit-directory");
        let path = dir.0.join(PROJECT_CONFIG_FILE);
        fs::create_dir(&path).unwrap();

        assert!(prepare_project_config_for_edit(&dir.0).is_err());
        assert!(path.is_dir());
    }

    #[test]
    fn edit_preparation_rejects_oversized_canonical_without_replacing_it() {
        let dir = TestDir::new("edit-oversized");
        let path = dir.0.join(PROJECT_CONFIG_FILE);
        let content = "x".repeat(MAX_CONFIG_BYTES as usize + 1);
        fs::write(&path, &content).unwrap();

        assert!(prepare_project_config_for_edit(&dir.0).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), content.len() as u64);
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }
}
