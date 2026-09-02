use super::IntegrationTarget;
use serde_json::{json, Map, Value};
use std::io;
use std::path::Path;

pub(crate) fn command(path: &Path, action: &str) -> String {
    format!(
        "'{}' {action}",
        path.display().to_string().replace('\'', "'\\''")
    )
}
fn object(content: &str, path: &Path) -> io::Result<Value> {
    if content.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(content)
        .map_err(|e| io::Error::other(format!("failed to parse {}: {e}", path.display())))?;
    if !value.is_object() {
        return Err(io::Error::other(format!(
            "{} must contain a JSON object",
            path.display()
        )));
    }
    Ok(value)
}
fn hooks(value: &mut Value) -> io::Result<&mut Map<String, Value>> {
    value
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| io::Error::other("hooks must be an object"))
}
fn push_unique(
    map: &mut Map<String, Value>,
    event: &str,
    entry: Value,
    command: &str,
) -> io::Result<()> {
    let list = map
        .entry(event.to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook {event} must be an array")))?;
    if !list.iter().any(|v| v.to_string().contains(command)) {
        list.push(entry);
    }
    Ok(())
}
fn remove_nested_actions(map: &mut Map<String, Value>, event: &str, path: &Path, actions: &[&str]) {
    let commands = actions
        .iter()
        .map(|action| command(path, action))
        .collect::<Vec<_>>();
    let Some(entries) = map.get_mut(event).and_then(Value::as_array_mut) else {
        return;
    };
    entries.retain_mut(|entry| {
        let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        hooks.retain(|hook| {
            !hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|value| commands.iter().any(|command| command == value))
        });
        !hooks.is_empty()
    });
    if entries.is_empty() {
        map.remove(event);
    }
}

fn nested(
    map: &mut Map<String, Value>,
    event: &str,
    path: &Path,
    action: &str,
    matcher: Option<&str>,
) -> io::Result<()> {
    let cmd = command(path, action);
    let mut e = json!({"hooks":[{"type":"command","command":cmd,"timeout":10}]});
    if let Some(m) = matcher {
        e.as_object_mut()
            .unwrap()
            .insert("matcher".into(), json!(m));
    }
    push_unique(map, event, e, &cmd)
}

pub(crate) fn json_config(
    target: IntegrationTarget,
    content: &str,
    config: &Path,
    hook: &Path,
) -> io::Result<String> {
    let mut root = object(content, config)?;
    match target {
        IntegrationTarget::Claude => {
            let hooks = hooks(&mut root)?;
            let events = [
                ("SessionStart", "idle"),
                ("UserPromptSubmit", "working"),
                ("PreToolUse", "working"),
                ("PostToolUse", "working"),
                ("PostToolUseFailure", "working"),
                ("PermissionRequest", "blocked"),
                ("Stop", "done"),
            ];
            for (event, _) in events {
                remove_nested_actions(
                    hooks,
                    event,
                    hook,
                    &["session", "idle", "working", "blocked", "done"],
                );
            }
            for (event, action) in events {
                nested(hooks, event, hook, action, Some("*"))?;
            }
        }
        IntegrationTarget::Codex
        | IntegrationTarget::Droid
        | IntegrationTarget::Qodercli
        | IntegrationTarget::Qwen => {
            nested(
                hooks(&mut root)?,
                "SessionStart",
                hook,
                "session",
                Some("*"),
            )?;
        }
        IntegrationTarget::Devin => {
            for event in [
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "PermissionRequest",
                "Stop",
            ] {
                nested(hooks(&mut root)?, event, hook, "session", None)?;
            }
        }
        IntegrationTarget::Copilot => {
            let cmd = command(hook, "session");
            let field = if cfg!(windows) { "powershell" } else { "bash" };
            let mut e = Map::new();
            e.insert("type".into(), json!("command"));
            e.insert(field.into(), json!(cmd));
            e.insert("timeoutSec".into(), json!(10));
            push_unique(hooks(&mut root)?, "SessionStart", Value::Object(e), &cmd)?;
        }
        IntegrationTarget::Cursor => {
            let cmd = command(hook, "session");
            push_unique(
                hooks(&mut root)?,
                "sessionStart",
                json!({"command":cmd}),
                &cmd,
            )?;
        }
        IntegrationTarget::Mastracode => {
            for (event, action) in [
                ("SessionStart", "idle"),
                ("UserPromptSubmit", "working"),
                ("AgentStart", "working"),
                ("PreToolUse", "working"),
                ("PermissionRequest", "blocked"),
                ("PermissionResult", "working"),
                ("SubagentStart", "working"),
                ("SubagentEnd", "working"),
                ("Interrupt", "idle"),
                ("AgentEnd", "done"),
                ("Stop", "done"),
            ] {
                let cmd = command(hook, action);
                push_unique(
                    root.as_object_mut().unwrap(),
                    event,
                    json!({"type":"command","command":cmd,"timeout":10000,"description":"Report MastraCode state to wsx"}),
                    &cmd,
                )?;
            }
        }
        IntegrationTarget::AntigravityCli => {
            let cmd = command(hook, "session");
            root.as_object_mut().unwrap().insert(
                "wsx".into(),
                json!({"PreInvocation":[{"type":"command","command":cmd,"timeout":10}]}),
            );
        }
        _ => return Err(io::Error::other("target has no editable JSON config")),
    }
    serde_json::to_string_pretty(&root)
        .map_err(io::Error::other)
        .map(|mut s| {
            s.push('\n');
            s
        })
}

pub(crate) fn codex_toml(content: &str) -> String {
    let mut lines: Vec<&str> = content
        .lines()
        .filter(|l| !l.trim_start().starts_with("codex_hooks"))
        .collect();
    let mut found = false;
    let mut in_features = false;
    for line in &mut lines {
        if line.trim_start().starts_with('[') {
            in_features = line.trim() == "[features]";
        }
        if in_features && line.trim_start().starts_with("hooks") {
            *line = "hooks = true";
            found = true;
        }
    }
    let mut out = lines.join("\n");
    if !found {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("[features]\nhooks = true");
    }
    out.push('\n');
    out
}

pub(crate) fn kimi_toml(content: &str, hook: &Path) -> String {
    const B: &str = "# >>> wsx kimi integration";
    const E: &str = "# <<< wsx kimi integration";
    let mut out = String::new();
    let mut skip = false;
    for l in content.lines() {
        if l.trim() == B {
            skip = true;
            continue;
        }
        if l.trim() == E {
            skip = false;
            continue;
        }
        if !skip {
            out.push_str(l);
            out.push('\n')
        }
    }
    if !out.trim().is_empty() {
        out.push('\n')
    }
    out.push_str(B);
    out.push('\n');
    for (event, matcher, action) in [
        ("SessionStart", None, "idle"),
        ("UserPromptSubmit", None, "working"),
        ("PreToolUse", Some("^(?!AskUserQuestion$).*$"), "working"),
        ("PreToolUse", Some("^AskUserQuestion$"), "blocked"),
        ("PostToolUse", Some("^AskUserQuestion$"), "working"),
        ("PostToolUseFailure", Some("^AskUserQuestion$"), "working"),
        ("SubagentStart", None, "working"),
        ("PreCompact", None, "working"),
        ("PermissionRequest", None, "blocked"),
        ("PermissionResult", None, "working"),
        ("Stop", None, "done"),
        ("Interrupt", None, "idle"),
    ] {
        out.push_str(&format!("[[hooks]]\nevent = \"{event}\"\n"));
        if let Some(matcher) = matcher {
            out.push_str(&format!("matcher = {:?}\n", matcher));
        }
        out.push_str(&format!(
            "command = {:?}\ntimeout = 10\n\n",
            command(hook, action)
        ));
    }
    out.push_str(E);
    out.push('\n');
    out
}

pub(crate) fn hermes_yaml(content: &str) -> String {
    if content
        .lines()
        .any(|line| line.trim().trim_start_matches("- ") == "wsx-agent-status")
    {
        return content.to_string();
    }

    let trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    if let Some(plugins) = lines.iter().position(|line| {
        !line.chars().next().is_some_and(char::is_whitespace)
            && line.trim_start().starts_with("plugins:")
    }) {
        if lines[plugins].trim() == "plugins: []" {
            lines.splice(
                plugins..=plugins,
                [
                    "plugins:".to_string(),
                    "  enabled:".to_string(),
                    "    - wsx-agent-status".to_string(),
                ],
            );
        } else {
            let end = lines[plugins + 1..]
                .iter()
                .position(|line| {
                    !line.chars().next().is_some_and(char::is_whitespace)
                        && !line.trim().is_empty()
                        && !line.trim_start().starts_with('#')
                })
                .map(|offset| plugins + 1 + offset)
                .unwrap_or(lines.len());
            if let Some(enabled) = lines[plugins + 1..end]
                .iter()
                .position(|line| line.starts_with("  ") && line.trim() == "enabled:")
                .map(|offset| plugins + 1 + offset)
            {
                lines.insert(enabled + 1, "    - wsx-agent-status".to_string());
            } else {
                lines.insert(plugins + 1, "  enabled:".to_string());
                lines.insert(plugins + 2, "    - wsx-agent-status".to_string());
            }
        }
    } else {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend([
            "plugins:".to_string(),
            "  enabled:".to_string(),
            "    - wsx-agent-status".to_string(),
        ]);
    }
    let mut output = lines.join("\n");
    if trailing_newline || !output.is_empty() {
        output.push('\n');
    }
    output
}
