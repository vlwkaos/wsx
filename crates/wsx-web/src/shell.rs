//! A `!` line in a session chat: the server runs it in the session's worktree and sends the
//! command and its output to the agent as the person's next message
//! (docs/web-client-chat.md "Shell lines").
use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use wsx_core::process::{run_bounded, Bound, Options, Stderr};

/// The longest command line the chat runs.
const MAX_LINE_BYTES: usize = 1024;
/// A command still running after this is stopped; long jobs belong in a terminal.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// The output the agent gets: the last part, which holds the result and the errors. Only
/// this much is kept while reading, so a command that prints without end cannot fill memory.
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// The command of a one-line `!` message, without the `!`. `!` alone, a second line or an
/// overlong line is a message.
pub fn command(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.len() > MAX_LINE_BYTES || text.contains(['\n', '\r']) {
        return None;
    }
    Some(text.strip_prefix('!')?.trim()).filter(|command| !command.is_empty())
}

pub struct Ran {
    /// `None`: stopped by a signal, also the timeout's.
    pub code: Option<i32>,
    pub elapsed: Duration,
    pub timed_out: bool,
    pub output: String,
    /// Bytes left out from the start of the output.
    pub cut: usize,
}

/// Run `command` with the person's login shell in `dir`, standard output and error together.
/// The command gets its own process group, so the timeout stops what it started too.
pub fn run(line: &str, dir: &Path) -> std::io::Result<Ran> {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let mut command = Command::new(shell);
    command
        .args(["-lc", line])
        .current_dir(dir)
        .stdin(Stdio::null());
    let ran = run_bounded(
        &mut command,
        &Options {
            timeout: TIMEOUT,
            stdout: Bound::Tail(MAX_OUTPUT_BYTES),
            stderr: Stderr::Merge,
            // ^ A background process the command left may still hold the pipe.
            reap: true,
        },
    )?;
    let bytes = ran.stdout;
    let dropped = ran.stdout_cut;
    // A cut can fall inside a character: start at the next whole one.
    let mut start = 0;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    Ok(Ran {
        code: ran.status.code(),
        elapsed: ran.elapsed,
        timed_out: ran.timed_out,
        output: String::from_utf8_lossy(&bytes[start..]).into_owned(),
        cut: dropped + start,
    })
}

/// The message the agent gets: what ran, where, how it ended, and the output.
pub fn message(command: &str, dir: &Path, ran: &Ran) -> String {
    let ended = match (ran.timed_out, ran.code) {
        (true, _) => format!("stopped after {} s", TIMEOUT.as_secs()),
        (false, Some(code)) => format!("exit {code}"),
        (false, None) => "ended by a signal".to_owned(),
    };
    let cut = match ran.cut {
        0 => String::new(),
        bytes => format!("; the first {bytes} bytes of output are left out"),
    };
    let output = ran.output.trim_end();
    // ^ Longer than any backtick run in the output, so the output cannot close the block.
    let longest = output.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let body = match output.is_empty() {
        true => "No output.".to_owned(),
        false => format!("{fence}\n{output}\n{fence}"),
    };
    format!(
        "I ran this in the shell in {}:\n\n$ {command}\n\n{ended} after {:.1} s{cut}.\n\n{body}",
        dir.display(),
        ran.elapsed.as_secs_f64(),
    )
}
