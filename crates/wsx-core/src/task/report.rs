//! `report.md` of one revision, built only from the log, git and the result.
//! ref: docs/tasks.md "Report"

use super::{LogEvent, MessageKind, TaskRecord, WorkerResult};
use crate::git::{git_cmd, output_with_timeout_limit};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

const GIT_OUTPUT_BYTES: usize = 32 * 1024;

fn git_text(repo: &Path, args: &[&str]) -> String {
    match output_with_timeout_limit(
        git_cmd(repo).args(args),
        Duration::from_secs(30),
        GIT_OUTPUT_BYTES,
    ) {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
        Ok(output) => format!(
            "(git {} failed: {})",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("(git {} failed: {error})", args.join(" ")),
    }
}

fn fenced(out: &mut String, text: &str) {
    let fence = if text.contains("```") { "~~~~" } else { "```" };
    let _ = writeln!(out, "{fence}\n{text}\n{fence}");
}

/// Render the report of revision `revision` at `commit`.
pub fn render(
    record: &TaskRecord,
    log: &[super::LogEntry],
    result: &WorkerResult,
    revision: u32,
    commit: &str,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Task t{} r{revision}: {}\n",
        record.id, record.project
    );
    let _ = writeln!(
        out,
        "State `{}`, branch `{}`, base `{}`, revision `{}`\n",
        record.state.as_str(),
        record.branch,
        short(&record.base),
        short(commit)
    );

    out.push_str("## Asked\n\n");
    for entry in log {
        if let LogEvent::Message { mode, text } = &entry.event {
            let label = match mode {
                MessageKind::Prompt => "Prompt",
                MessageKind::Steer => "Steer",
                MessageKind::Add => "Add",
            };
            let _ = writeln!(out, "**{label}**\n");
            fenced(&mut out, text);
            out.push('\n');
        }
    }

    let _ = writeln!(out, "## Summary\n\nWorker status: `{}`\n", result.status);
    let _ = writeln!(out, "{}\n", nonempty(&result.summary));

    out.push_str("## Decisions\n\n");
    if result.decisions.is_empty() {
        out.push_str("None reported.\n\n");
    }
    for decision in &result.decisions {
        let _ = writeln!(out, "- {}: {}", decision.choice, decision.reason);
    }
    if !result.decisions.is_empty() {
        out.push('\n');
    }

    out.push_str("## Changes\n\n");
    let range = format!("{}..{commit}", record.base);
    let commits = git_text(
        &record.working_copy,
        &["log", "--oneline", "--no-decorate", &range],
    );
    let stat = git_text(&record.working_copy, &["diff", "--stat", &range]);
    if commits.is_empty() {
        out.push_str("No commits.\n\n");
    } else {
        fenced(&mut out, &commits);
        out.push('\n');
        fenced(&mut out, &stat);
        out.push('\n');
    }
    let dirty = git_text(&record.working_copy, &["status", "--short"]);
    if !dirty.is_empty() {
        out.push_str("Not committed:\n\n");
        fenced(&mut out, &dirty);
        out.push('\n');
    }

    out.push_str("## Evidence\n\n");
    let checks = log
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            LogEvent::Checks { results } => Some(results),
            _ => None,
        })
        .filter(|results| !results.is_empty());
    match checks {
        None => out.push_str("No checks are declared (`tasks.checks`).\n\n"),
        Some(results) => {
            for check in results {
                let outcome = match (check.timed_out, check.exit) {
                    (true, _) => "timed out".to_string(),
                    (false, Some(code)) => format!("exit {code}"),
                    (false, None) => "killed by a signal".to_string(),
                };
                let _ = writeln!(out, "- `{}`: {outcome}", check.command);
                if check.exit != Some(0) && !check.output.is_empty() {
                    out.push('\n');
                    fenced(&mut out, &check.output);
                }
            }
            out.push('\n');
        }
    }

    out.push_str("## Human Verify\n\n");
    if result.human_verify.is_empty() {
        out.push_str("None reported.\n\n");
    }
    for item in &result.human_verify {
        let _ = writeln!(out, "- [ ] {} (pass if: {})", item.check, item.pass_if);
    }
    if !result.human_verify.is_empty() {
        out.push('\n');
    }

    out.push_str("## Left open\n\n");
    if result.left_open.is_empty() {
        out.push_str("Nothing reported.\n");
    }
    for item in &result.left_open {
        let _ = writeln!(out, "- {item}");
    }
    if !result.notes.is_empty() {
        let _ = writeln!(out, "\nNotes: {}", result.notes);
    }
    out
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn nonempty(text: &str) -> &str {
    if text.trim().is_empty() {
        "(none)"
    } else {
        text
    }
}
