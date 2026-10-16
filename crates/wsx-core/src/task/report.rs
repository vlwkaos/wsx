//! `report.md` of one revision, built only from the log, git and the result.
//! ref: docs/tasks.md "Report"

use super::{LogEntry, LogEvent, MessageKind, TaskRecord, Verdict, WorkerResult};
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
                MessageKind::Verify => "Human verification (fail)",
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

    out.push_str("## Human Verify\n\n");
    let verdict = super::verification(log, revision);
    let passed = super::passed(log, revision);
    if result.human_verify.is_empty() {
        out.push_str("None reported.\n\n");
    }
    for item in &result.human_verify {
        let mark = if passed { 'x' } else { ' ' };
        let _ = writeln!(out, "- [{mark}] {} (pass if: {})", item.check, item.pass_if);
    }
    if !result.human_verify.is_empty() {
        out.push('\n');
    }
    match verdict {
        Some(LogEntry {
            unix_ms,
            event:
                LogEvent::Verified {
                    result,
                    note,
                    carried_from,
                    ..
                },
        }) => {
            let result = match result {
                Verdict::Pass => "pass",
                Verdict::Fail => "fail",
            };
            let _ = write!(
                out,
                "Verdict on r{revision}: **{result}** at {}",
                utc(*unix_ms)
            );
            if let Some(from) = carried_from {
                let _ = write!(
                    out,
                    " (carried from r{from}; the default branch changed none of this task's paths)"
                );
            }
            out.push('\n');
            if let Some(note) = note {
                out.push('\n');
                fenced(&mut out, note);
            }
            out.push('\n');
        }
        _ if result.human_verify.is_empty() => {}
        _ => {
            let _ = writeln!(out, "Verdict on r{revision}: not verified\n");
        }
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

/// `YYYY-MM-DD HH:MM UTC` from Unix milliseconds (days-to-civil, proleptic Gregorian).
fn utc(unix_ms: u64) -> String {
    let seconds = unix_ms / 1000;
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest / 60 % 60
    )
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
