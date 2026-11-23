//! `report.md` of one revision, built only from the log, git and the result.
//! ref: docs/tasks.md "Report"

use super::view::short_commit;
use super::{LogEvent, TaskRecord, WorkerResult};
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

const CHECK_DIFF_BYTES: usize = 256 * 1024;
const SHOWN_CHECKS: usize = 10;

/// An `assert` that starts a word (Python `assert`, Rust `assert!`/`assert_eq!`, JS
/// `assert.equal`), outside a line comment.
fn is_assert(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('#') || line.starts_with("//") {
        return false;
    }
    line.match_indices("assert").any(|(at, _)| {
        !line[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// "Edited test checks": the `assert` lines the revision removed or changed, so the reviewer sees
/// a change that made its tests pass by editing them. Added asserts are not listed.
/// ref: docs/tasks.md "Checks, revisions and report"
fn edited_checks(out: &mut String, repo: &Path, range: &str) {
    let diff = match output_with_timeout_limit(
        // ^ Paths as written: no `a/` or mnemonic prefix, no quoting (`diff.mnemonicPrefix`,
        // `core.quotePath` of the owner's config).
        git_cmd(repo).args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-prefix",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "-Gassert",
            range,
        ]),
        Duration::from_secs(30),
        CHECK_DIFF_BYTES,
    ) {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        Ok(_) | Err(_) => {
            let _ = writeln!(
                out,
                "Edited test checks: unknown, the diff could not be read (`git diff -Gassert {range}`).\n"
            );
            return;
        }
    };
    let mut file = "";
    // ^ A file's header runs from `diff --git` to its first hunk; only there is `--- ` the old
    // path, since a removed line may itself start with `--`.
    let mut header = false;
    let mut edited: Vec<(&str, &str)> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            header = true;
        } else if line.starts_with("@@") {
            header = false;
        } else if header {
            // ^ git ends a path that has a space with a tab.
            if let Some(path) = line.strip_prefix("--- ") {
                file = path.trim_end_matches('\t');
            }
        } else if let Some(removed) = line.strip_prefix('-').filter(|text| is_assert(text)) {
            edited.push((file, removed.trim()));
        }
    }
    if edited.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "Edited test checks: {} `assert` line{} removed or changed (`git diff -Gassert {range}`):\n",
        edited.len(),
        if edited.len() == 1 { "" } else { "s" }
    );
    for (file, text) in edited.iter().take(SHOWN_CHECKS) {
        let text: String = text.chars().take(160).collect();
        let _ = writeln!(out, "- `{file}`: `{}`", text.replace('`', "'"));
    }
    if edited.len() > SHOWN_CHECKS {
        let _ = writeln!(out, "- +{} more", edited.len() - SHOWN_CHECKS);
    }
    out.push('\n');
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
        short_commit(&record.base),
        short_commit(commit)
    );

    let _ = writeln!(out, "## Summary\n\nWorker status: `{}`\n", result.status);
    let _ = writeln!(out, "{}\n", nonempty(&result.summary));
    // ^ Decisions belong to the summary when there are any (docs/tasks.md "Checks, revisions and report").
    if !result.decisions.is_empty() {
        let decided: Vec<String> = result
            .decisions
            .iter()
            .map(|decision| format!("{} ({})", decision.choice, decision.reason))
            .collect();
        let _ = writeln!(out, "Decided: {}.\n", decided.join("; "));
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
    edited_checks(&mut out, &record.working_copy, &range);
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
            let passed = results.iter().filter(|check| check.exit == Some(0)).count();
            let _ = writeln!(out, "{}\n", checks_line(passed, results.len()));
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
    if !result.evidence.is_empty() {
        out.push_str("The worker showed:\n\n");
        for item in &result.evidence {
            let _ = writeln!(out, "- {}: `{}`", item.check, item.proof);
        }
        out.push('\n');
    }

    let denials = log.iter().rev().find_map(|entry| match &entry.event {
        LogEvent::Result { denials, .. } => Some(denials),
        _ => None,
    });
    if let Some(denials) = denials.filter(|denials| !denials.is_empty()) {
        out.push_str("Denied tool calls in the last turn (the worker continued without them):\n\n");
        for denial in denials {
            let _ = writeln!(out, "- `{denial}`");
        }
        out.push('\n');
    }

    out.push_str("## To check\n\n");
    if result.human_verify.is_empty() {
        out.push_str("Nothing needs a person.\n\n");
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

/// "4 checks passed", or "3 of 4 checks passed".
pub fn checks_line(passed: usize, total: usize) -> String {
    let noun = if total == 1 { "check" } else { "checks" };
    if passed == total {
        format!("{total} {noun} passed")
    } else {
        format!("{passed} of {total} {noun} passed")
    }
}

fn nonempty(text: &str) -> &str {
    if text.trim().is_empty() {
        "(none)"
    } else {
        text
    }
}
