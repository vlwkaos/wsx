//! What a blocked agent asks, and the keys that answer it (docs/web-client-chat.md "Answering").
//! ^ wsxd knows that an agent is blocked, not what it asks. The question is the dialog
//! the agent drew, so this is the one place of the web server that reads a pane's
//! screen. No provider dialog is modelled here: the page shows the rows and sends keys.
use crate::{
    chat::{call, Fault, Outcome},
    devices::hex,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use wsx_core::{
    integration::memory::{Item, ToolStatus},
    runtime::{self, Client, KeyCode, KeyEvent, PaneId, Request, Response},
};

const MAX_ROWS: usize = 24;
const MAX_BYTES: usize = 4096;
const MAX_ROW_CHARS: usize = 240;
const MAX_LABEL_CHARS: usize = 120;
pub const MAX_TEXT_CHARS: usize = 2000;

/// The end of the pane's screen as plain text: at most 24 rows and 4 KiB, without
/// control characters, without empty rows at either end.
fn screen(client: &Client, pane: PaneId) -> Outcome<Vec<String>> {
    let frame = match call(
        client,
        &Request::View {
            pane_ids: vec![pane],
        },
    )? {
        Response::View { mut frames, .. } => frames.pop(),
        _ => None,
    }
    .ok_or_else(|| Fault::protocol("the terminal frame is unavailable"))?;
    let mut rows: Vec<String> = frame
        .cells
        .chunks(usize::from(frame.cols).max(1))
        .map(|row| {
            let text: String = row
                .iter()
                .flat_map(|cell| cell.symbol.chars())
                .filter(|character| !character.is_control())
                .take(MAX_ROW_CHARS)
                .collect();
            text.trim_end().to_owned()
        })
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    let mut rows = rows.split_off(rows.len().saturating_sub(MAX_ROWS));
    while rows.iter().map(|row| row.len() + 1).sum::<usize>() > MAX_BYTES {
        rows.remove(0);
    }
    let first = rows.iter().position(|row| !row.is_empty()).unwrap_or(0);
    Ok(rows.split_off(first))
}

/// The last `count` rows of the screen, for a hold the chat cannot name
/// (docs/web-client-chat.md "Timeline"). `None` when the screen cannot be read.
pub fn tail(client: &Client, pane: PaneId, count: usize) -> Option<Vec<String>> {
    let mut rows = screen(client, pane).ok()?;
    Some(rows.split_off(rows.len().saturating_sub(count)))
}

fn stamp_of(rows: &[String]) -> String {
    hex(&Sha256::digest(rows.join("\n"))[..16])
}

/// A hash of the screen as a question would show it now. `None` when it cannot be read.
pub fn stamp(client: &Client, pane: PaneId) -> Option<String> {
    screen(client, pane).ok().map(|rows| stamp_of(&rows))
}

/// One generic rule for numbered dialogs: a row that starts, after a box edge and an
/// optional cursor mark, with `1.` to `9.` and a label. Only the last list that counts
/// up from 1 is taken, so a numbered list in the agent's earlier text is not offered.
fn options(rows: &[String]) -> Vec<Value> {
    let numbered = |row: &String| -> Option<(u32, String)> {
        let text = row.trim_start_matches(|character: char| {
            character.is_whitespace() || "│┃|❯›>▶▸●◉→*".contains(character)
        });
        let mut characters = text.chars();
        let key = characters.next()?.to_digit(10).filter(|key| *key >= 1)?;
        let rest = characters.as_str().strip_prefix('.')?;
        let label: String = rest
            .trim_end_matches(|character: char| {
                character.is_whitespace() || "│┃|".contains(character)
            })
            .trim()
            .chars()
            .take(MAX_LABEL_CHARS)
            .collect();
        (rest.starts_with(' ') && !label.is_empty()).then_some((key, label))
    };
    let found: Vec<(u32, String)> = rows.iter().filter_map(numbered).collect();
    let Some(start) = found.iter().rposition(|(key, _)| *key == 1) else {
        return Vec::new();
    };
    found[start..]
        .iter()
        .zip(1..)
        .take_while(|((key, _), expected)| key == expected)
        .map(|((key, label), _)| json!({ "key": key.to_string(), "label": label }))
        .collect()
}

/// The timeline's `question`: the dialog rows, the choices found in them, and the tool
/// call that waits, when the saved conversation ends with one.
pub fn view(client: &Client, pane: PaneId, last: Option<&Item>) -> Option<Value> {
    let rows = screen(client, pane).ok()?;
    let request = last
        .and_then(|item| item.tool.as_ref())
        .filter(|tool| tool.status == ToolStatus::Running)
        .map(|tool| json!({ "name": tool.name, "title": tool.title }));
    Some(json!({
        "stamp": stamp_of(&rows),
        "options": options(&rows),
        "screen": rows,
        "request": request,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    stamp: String,
    key: String,
    #[serde(default)]
    text: Option<String>,
}

enum Keys {
    Key(KeyEvent),
    /// Typed text, then Enter, as one write.
    Bytes(Vec<u8>),
}

impl Answer {
    fn keys(&self) -> Outcome<Keys> {
        let invalid = || {
            Fault::new(
                400,
                "invalid_answer",
                "key must be 1 to 9, enter, escape, up, down, tab, or text with at most 2000 characters",
            )
        };
        let key = |code, text: &str| KeyEvent {
            code,
            text: text.to_owned(),
            shift: false,
            control: false,
            alt: false,
            super_key: false,
            repeat: false,
        };
        if (self.key == "text") != self.text.is_some() {
            return Err(invalid());
        }
        Ok(match self.key.as_str() {
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                Keys::Key(key(KeyCode::Text, &self.key))
            }
            "enter" => Keys::Key(key(KeyCode::Enter, "")),
            "escape" => Keys::Key(key(KeyCode::Escape, "")),
            "up" => Keys::Key(key(KeyCode::Up, "")),
            "down" => Keys::Key(key(KeyCode::Down, "")),
            "tab" => Keys::Key(key(KeyCode::Tab, "")),
            "text" => {
                // One line: a control character could be a key nobody chose.
                let text: String = self
                    .text
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .filter(|character| !character.is_control())
                    .collect();
                let text = text.trim();
                if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
                    return Err(invalid());
                }
                Keys::Bytes(format!("{text}\r").into_bytes())
            }
            _ => return Err(invalid()),
        })
    }
}

/// Send one answer to a blocked agent's pane. The caller checked the grant, the
/// capability and that the agent is blocked.
/// ^ The lease comes first, so nobody else types between the check of the stamp and the
/// write. It takes the pane from a terminal or a wsx window that holds it: answering is
/// the person's own action, and the displaced controller says that control moved.
pub fn answer(client: &Client, pane: PaneId, answer: &Answer) -> Outcome<Value> {
    let keys = answer.keys()?;
    with_lease(client, pane, |client_id| {
        if screen(client, pane).map(|rows| stamp_of(&rows))? != answer.stamp {
            return Err(Fault::new(
                409,
                "question_changed",
                "the question changed; read it again",
            ));
        }
        let request = match keys {
            Keys::Key(key) => Request::TerminalKey {
                pane_id: pane,
                client_id,
                key,
            },
            Keys::Bytes(bytes) => Request::TerminalInput {
                pane_id: pane,
                client_id,
                bytes,
            },
        };
        call(client, &request).map(|_| json!({ "sent": true }))
    })
}

/// Take the pane's lease for one person's action, run `act` with it, then let it go. It
/// takes the pane from a terminal or a wsx window that holds it.
pub fn with_lease<T>(
    client: &Client,
    pane: PaneId,
    act: impl FnOnce(u64) -> Outcome<T>,
) -> Outcome<T> {
    let client_id = runtime::new_client_id();
    match client.call(&Request::TerminalAcquire {
        pane_id: pane,
        client_id,
        takeover: true,
    }) {
        Ok(Response::Ack { .. }) => {}
        Ok(Response::Error(error)) => {
            return Err(Fault::new(
                409,
                "daemon_refused",
                format!("{}: {}", error.code, error.message),
            ))
        }
        Ok(_) => return Err(Fault::protocol("unexpected lease response")),
        Err(error) => {
            return Err(Fault::new(
                503,
                "daemon_unavailable",
                format!("wsxd is not reachable: {error}"),
            ))
        }
    }
    let done = act(client_id);
    let _ = client.call(&Request::TerminalRelease {
        pane_id: pane,
        client_id,
    });
    done
}
