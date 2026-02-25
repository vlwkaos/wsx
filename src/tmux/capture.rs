// tmux capture-pane for preview panel

use super::tmux_cmd;

pub fn capture_pane(session_name: &str) -> Option<String> {
    let output = tmux_cmd(&["capture-pane", "-t", session_name, "-p", "-e"])
        .output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

pub fn trim_capture(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let last_nonempty = lines.iter().rposition(|l| !l.trim().is_empty());
    match last_nonempty {
        Some(i) => lines[..=i].join("\n"),
        None => String::new(),
    }
}
