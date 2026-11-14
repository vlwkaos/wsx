//! The local model floor: the built-in stand-in, known to wsx without an `[[agents]]` entry or
//! a list edit. It counts only while its endpoint answers. Serving
//! belongs to kaos; wsx starts no server. ref: docs/local-model.md, docs/tasks.md "Agents"

use super::agent::{self, Choice};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wsx_core::config::global::AgentEntry;
use wsx_core::task::policy::Capability;

/// The Pi provider name of the floor in the per-launch `models.json`.
pub const PROVIDER: &str = "llama-wsx";
const NAME: &str = "local-model";
const DEFAULT_URL: &str = "http://127.0.0.1:8090";
const DEFAULT_MODEL: &str = "lfm3-1.7b";
/// How long an answer (or no answer) of the endpoint holds.
const CACHE: Duration = Duration::from_secs(15);
const CONNECT: Duration = Duration::from_secs(1);
const IO: Duration = Duration::from_secs(3);
/// What the floor passed: the stand-in's `answer`, no more. ref: docs/tasks.md "Agents"
const MAY: [Capability; 1] = [Capability::Answer];

/// The endpoint's settings: `LOCAL_MODEL_URL`, `LOCAL_MODEL_KEY_FILE` and `LOCAL_MODEL_ID`,
/// with the defaults of docs/local-model.md.
struct Endpoint {
    url: String,
    key_file: PathBuf,
    id: String,
}

fn endpoint() -> Endpoint {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    Endpoint {
        url: env("LOCAL_MODEL_URL")
            .unwrap_or_else(|| DEFAULT_URL.into())
            .trim_end_matches('/')
            .into(),
        key_file: env("LOCAL_MODEL_KEY_FILE").map_or_else(
            || {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".local/state/local-model/api-key")
            },
            PathBuf::from,
        ),
        id: env("LOCAL_MODEL_ID").unwrap_or_else(|| DEFAULT_MODEL.into()),
    }
}

/// The floor as a registry entry; `agent::registry` adds it unless the owner's file already
/// runs the same adapter and model.
pub fn entry() -> AgentEntry {
    AgentEntry::built_in(
        NAME,
        "pi",
        &format!("{PROVIDER}/{}", endpoint().id),
        MAY.into(),
    )
}

/// `GET path` on `url` (plain http): the status and the body, or none with no answer.
fn get(url: &str, path: &str, key: Option<&str>) -> Option<(u16, String)> {
    let authority = url.strip_prefix("http://")?.split('/').next()?;
    let with_port = match authority.contains(':') {
        true => authority.to_string(),
        false => format!("{authority}:80"),
    };
    let addr = with_port.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT).ok()?;
    stream.set_read_timeout(Some(IO)).ok()?;
    stream.set_write_timeout(Some(IO)).ok()?;
    let auth = key.map_or(String::new(), |key| {
        format!("Authorization: Bearer {key}\r\n")
    });
    // ^ HTTP/1.0: the answer is not chunked and ends with the connection.
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {authority}\r\n{auth}\r\n"
    )
    .ok()?;
    let mut text = Vec::new();
    stream.take(64 * 1024).read_to_end(&mut text).ok()?;
    let text = String::from_utf8_lossy(&text);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, body.to_string()))
}

fn key_of(file: &Path) -> Option<String> {
    let key = std::fs::read_to_string(file).ok()?;
    Some(key.trim().to_string()).filter(|key| !key.is_empty())
}

/// Whether `/health` answers and `/v1/models`, with the key, lists the model.
fn answers(endpoint: &Endpoint) -> bool {
    let Some(key) = key_of(&endpoint.key_file) else {
        return false;
    };
    let health = get(&endpoint.url, "/health", None);
    if health.is_none_or(|(status, _)| status != 200) {
        return false;
    }
    let Some((200, body)) = get(&endpoint.url, "/v1/models", Some(&key)) else {
        return false;
    };
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|models| {
            models.get("data")?.as_array().map(|data| {
                data.iter()
                    .any(|model| model.get("id").and_then(Value::as_str) == Some(&endpoint.id))
            })
        })
        .unwrap_or(false)
}

/// `answers`, remembered briefly: a limit's hold asks on every tick.
fn up(endpoint: &Endpoint) -> bool {
    static LAST: Mutex<Option<(String, Instant, bool)>> = Mutex::new(None);
    // ^ A test shortens the memory so it can take the endpoint down and up.
    let keep = std::env::var("WSX_TEST_FLOOR_CACHE_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map_or(CACHE, Duration::from_secs);
    let key = format!("{} {}", endpoint.url, endpoint.id);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((seen, at, up)) = &*last {
        if *seen == key && at.elapsed() < keep {
            return *up;
        }
    }
    let up = answers(endpoint);
    *last = Some((key, Instant::now(), up));
    up
}

/// The floor, when its endpoint answers and its account has no limit or failed sign-in on
/// record. ref: docs/tasks.md "Agents"
pub fn choice(now_ms: u64) -> Option<Choice> {
    let endpoint = endpoint();
    let entry = entry();
    let (adapter, model) = (entry.adapter?, entry.model?);
    if agent::blocked_until(&agent::account(&adapter, &model), now_ms).is_some() || !up(&endpoint) {
        return None;
    }
    Some(Choice {
        name: entry.name,
        adapter,
        model,
    })
}

/// Why no floor answers, for the line that says Claude is waited for.
pub fn down_note() -> String {
    format!(
        "no stand-in answers; the local model does not answer at {}",
        endpoint().url
    )
}

/// The endpoint's URL for Pi provider `provider`, when it is the floor's.
pub fn url_of(provider: &str) -> Option<String> {
    (provider == PROVIDER).then(|| endpoint().url)
}

/// Give a launch's `models.json` the floor's provider when `model` is the floor's and the file
/// lacks one by that name: an owner's own `llama-wsx` wins. The file is the launch's copy,
/// mode 600, gone with the launch. ref: docs/design/local-model-fallback.md 2.6
pub fn add_provider(models_json: &Path, model: &str) -> Result<()> {
    let Some(id) = model.strip_prefix(&format!("{PROVIDER}/")) else {
        return Ok(());
    };
    let mut file = match std::fs::read_to_string(models_json) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .with_context(|| format!("{} is not JSON", models_json.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error).context("cannot read Pi's models.json"),
    };
    let providers = file
        .as_object_mut()
        .context("models.json is not an object")?
        .entry("providers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("models.json providers is not an object")?;
    if providers.contains_key(PROVIDER) {
        return Ok(());
    }
    let endpoint = endpoint();
    let key = key_of(&endpoint.key_file).context("the local model's key file is missing")?;
    providers.insert(
        PROVIDER.into(),
        json!({
            "baseUrl": format!("{}/v1", endpoint.url),
            "api": "openai-completions",
            "apiKey": key,
            "models": [{
                "id": id, "name": "Local model (wsx floor)", "reasoning": false,
                "input": ["text"], "contextWindow": 32768, "maxTokens": 2048,
                "compat": { "supportsStore": false, "maxTokensField": "max_tokens" }
            }]
        }),
    );
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(models_json)
        .with_context(|| format!("cannot write {}", models_json.display()))?;
    out.write_all(file.to_string().as_bytes())
        .context("cannot write Pi's models.json")
}
