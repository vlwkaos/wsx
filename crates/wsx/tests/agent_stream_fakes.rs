//! The stand-in agents of the task scenarios (`scripts/fake-claude-stream.py`,
//! `scripts/fake-pi-rpc.py`) write the lines the real CLIs wrote in the recordings under
//! `tests/fixtures/agent-streams` (docs/tasks.md "Agent stream recordings"). A fake whose lines
//! drift from the recording, or invent a field, or leave one out, fails here, so a task scenario
//! never passes on a stream the real CLI does not send. Drives the fakes as real processes.

use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

/// Maps whose keys are the data's own (tool input, per-model usage): their fields are no shape.
const OPEN: &[&str] = &[
    "input",
    "arguments",
    "args",
    "tool_use_result",
    "structured_output",
    "wire_tool_inputs",
    "modelUsage",
    "model_usage_since_last_result",
    "subagent_stats",
    "memory_paths",
    "sections",
    "unifiedWindows",
];

type Shape = BTreeMap<String, BTreeSet<&'static str>>;

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Every field path of a line with its types. Arrays are leaves except message `content`, whose
/// blocks are the lines' real structure.
fn shape(value: &Value, path: &str, out: &mut Shape) {
    out.entry(path.to_string()).or_default().insert(kind(value));
    match value {
        Value::Object(fields) => {
            for (name, field) in fields {
                let at = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}.{name}")
                };
                if OPEN.contains(&name.as_str()) {
                    out.entry(at).or_default().insert(kind(field));
                } else {
                    shape(field, &at, out);
                }
            }
        }
        Value::Array(items) if path.ends_with("content") => {
            for item in items {
                shape(item, &format!("{path}[]"), out);
            }
        }
        _ => {}
    }
}

/// What kind of line this is: its type and subtype, the message's role, stop reason and content
/// block types.
fn class(line: &Value) -> String {
    let text = |pointer: &str| line.pointer(pointer).and_then(Value::as_str).unwrap_or("");
    let blocks: Vec<&str> = line
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|block| block["type"].as_str().unwrap_or(""))
        .collect();
    format!(
        "{}/{}/{}/{}/{}",
        text("/type"),
        text("/subtype"),
        text("/message/role"),
        text("/message/stopReason"),
        blocks.join("+")
    )
}

/// Per line class: every path any recording has, and the paths all of them have.
#[derive(Default)]
struct Recorded {
    seen: Shape,
    always: Option<BTreeSet<String>>,
}

fn recorded(adapter: &str) -> BTreeMap<String, Recorded> {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent-streams");
    let mut classes: BTreeMap<String, Recorded> = BTreeMap::new();
    for file in fs::read_dir(folder.join(adapter)).expect("the recordings") {
        let text = fs::read_to_string(file.expect("a recording").path()).expect("read a recording");
        for line in text.lines() {
            let line: Value = serde_json::from_str(line).expect("a recorded line is JSON");
            let mut fields = Shape::new();
            shape(&line, "", &mut fields);
            let entry = classes.entry(class(&line)).or_default();
            let paths: BTreeSet<String> = fields.keys().cloned().collect();
            entry.always = Some(match entry.always.take() {
                Some(always) => always.intersection(&paths).cloned().collect(),
                None => paths,
            });
            for (path, types) in fields {
                entry.seen.entry(path).or_default().extend(types);
            }
        }
    }
    classes
}

/// Run `argv` (a python script), send `messages`, and collect stdout lines until `done` accepts
/// one per message.
fn drive(argv: &[&str], home: &Path, messages: &[Value], done: fn(&Value) -> bool) -> Vec<Value> {
    fs::create_dir_all(home).expect("create the home");
    let mut child = Command::new("python3")
        .arg("-B")
        .args(argv)
        .current_dir(home)
        .env("HOME", home)
        .env("FAKE_CLAUDE_TRACE", home.join("trace"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start python3 (a fake agent)");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    let mut seen = Vec::new();
    for message in messages {
        writeln!(stdin, "{message}").expect("send a message");
        loop {
            let line = lines
                .recv_timeout(Duration::from_secs(30))
                .expect("the fake answers in time");
            let line: Value = serde_json::from_str(&line).expect("a fake line is JSON");
            let last = done(&line);
            seen.push(line);
            if last {
                break;
            }
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    seen
}

/// Fail on a line of an unrecorded class, a field the recording never has, a type it never has,
/// or a field every recorded line of that class has.
fn compare(adapter: &str, fake: &[Value], must_cover: &[&str]) {
    let recordings = recorded(adapter);
    let mut problems = BTreeSet::new();
    let mut covered = BTreeSet::new();
    for line in fake {
        let name = class(line);
        let Some(recording) = recordings.get(&name) else {
            problems.insert(format!("{name}: no recording has this kind of line"));
            continue;
        };
        covered.insert(name.clone());
        let mut fields = Shape::new();
        shape(line, "", &mut fields);
        for (path, types) in &fields {
            let Some(real) = recording.seen.get(path) else {
                problems.insert(format!("{name}: the recording has no field `{path}`"));
                continue;
            };
            // ^ null stands for "absent value" on either side.
            let fake_types: Vec<_> = types.iter().filter(|kind| **kind != "null").collect();
            let real_types: Vec<_> = real.iter().filter(|kind| **kind != "null").collect();
            if !fake_types.is_empty()
                && !real_types.is_empty()
                && fake_types.iter().any(|kind| !real_types.contains(kind))
            {
                problems.insert(format!(
                    "{name}: `{path}` is {types:?}, the recording's is {real:?}"
                ));
            }
        }
        for path in recording.always.iter().flatten() {
            if !fields.contains_key(path) {
                problems.insert(format!("{name}: the fake leaves out `{path}`"));
            }
        }
    }
    for name in must_cover {
        if !covered.contains(*name) {
            problems.insert(format!("{name}: the fake writes no such line"));
        }
    }
    assert!(
        problems.is_empty(),
        "the {adapter} fake drifts from the recordings:\n{}",
        problems.into_iter().collect::<Vec<_>>().join("\n")
    );
}

fn scratch(name: &str) -> PathBuf {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("fakes-{name}"));
    let _ = fs::remove_dir_all(&home);
    home
}

fn user(text: &str) -> Value {
    json!({ "type": "user", "message": { "role": "user", "content": text } })
}

#[test]
fn fake_claude_writes_the_recorded_lines() {
    let script = repository().join("scripts/fake-claude-stream.py");
    let script = script.to_str().expect("UTF-8 path");
    let is_result = |line: &Value| line["type"] == "result";
    // ^ A manager turn: text, a tool call and its result, more text, a subagent's call.
    let manager = drive(
        &[
            script,
            "-p",
            "--model",
            "fake-manager",
            "--session-id",
            "demo-session",
            "--append-system-prompt",
            "You are the manager of wsx project demo. The project overview page.md is yours.",
        ],
        &scratch("claude-manager"),
        &[user("BLOCKS First, the answer.|TOOL|Then, the close.")],
        is_result,
    );
    // ^ A worker turn: a structured result.
    let worker = drive(
        &[
            script,
            "-p",
            "--model",
            "fake-model",
            "--session-id",
            "work-session",
            "--json-schema",
            r#"{"type":"object"}"#,
        ],
        &scratch("claude-worker"),
        &[user("NOOP")],
        is_result,
    );
    let lines: Vec<Value> = manager.into_iter().chain(worker).collect();
    compare(
        "claude",
        &lines,
        &[
            "system/init///",
            "assistant//assistant//text",
            "assistant//assistant//tool_use",
            "assistant//assistant//thinking",
            "user//user//tool_result",
            "result/success///",
        ],
    );
}

#[test]
fn fake_pi_writes_the_recorded_lines() {
    let script = repository().join("scripts/fake-pi-rpc.py");
    let home = scratch("pi");
    fs::create_dir_all(&home).expect("create the home");
    let refusal = home.join("refusal.txt");
    fs::write(&refusal, "Your credit balance is too low.").expect("write the refusal");
    let prompt = |message: String| json!({ "type": "prompt", "id": "1", "message": message });
    let lines = drive(
        &[
            script.to_str().expect("UTF-8 path"),
            "--mode",
            "rpc",
            "--model",
            "fake/pi-model",
            "--session-id",
            "pi-session",
        ],
        &home,
        &[
            prompt("Say hello".into()),
            prompt(format!("PIERROR {}", refusal.display())),
        ],
        |line| line["type"] == "agent_settled",
    );
    compare(
        "pi",
        &lines,
        &[
            "response////",
            "agent_start////",
            "tool_execution_start////",
            "message_end//assistant/stop/text",
            "message_end//assistant/error/",
            "agent_settled////",
        ],
    );
}
