use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use wsx_core::{integration::memory::Reader, runtime::AgentSessionRef};
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work")
            .join(format!(
                "mem-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn reference(&self, body: &str) -> AgentSessionRef {
        let path = self.0.join("session.jsonl");
        fs::write(&path, body).unwrap();
        AgentSessionRef::id("session")
            .unwrap()
            .with_transcript_path(path.to_str().unwrap().into())
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn native_branch_projection_excludes_abandoned_tools_reasoning_and_other_sessions() {
    let fixture = Fixture::new();
    let reference = fixture.reference(concat!(
        "{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"sessionId\":\"session\",\"message\":{\"content\":\"question\"}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"old\",\"parentUuid\":\"u\",\"message\":{\"content\":\"abandoned answer\"}}\n",
        "{\"type\":\"user\",\"uuid\":\"summary\",\"parentUuid\":\"u\",\"isCompactSummary\":true,\"message\":{\"content\":\"retained decision\"}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"a\",\"parentUuid\":\"summary\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"private reasoning\"},{\"type\":\"tool_use\",\"input\":\"secret tool\"},{\"type\":\"text\",\"text\":\"current answer\"}]}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"other\",\"parentUuid\":null,\"sessionId\":\"another-session\",\"message\":{\"content\":\"wrong session\"}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"side\",\"isSidechain\":true,\"message\":{\"content\":\"side agent\"}}\n"
    ));
    let memory = Reader::default().read("claude", Some(&reference), 8, 8192);
    assert_eq!(
        memory
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["question", "current answer"]
    );
    assert_eq!(
        memory.checkpoint.as_ref().unwrap().text,
        "retained decision"
    );
    let small = Reader::default().read("claude", Some(&reference), 1, 7);
    assert_eq!(small.messages[0].text, "curr");
    assert_eq!(small.checkpoint.as_ref().unwrap().text, "ret");
    assert!(small.truncated);
    let complete = Reader::default().read("claude", Some(&reference), 8, 8192);
    assert!(!complete.truncated);
}

#[test]
fn pi_compaction_and_codex_native_records_use_actual_text_only() {
    let fixture = Fixture::new();
    let reference = fixture.reference(concat!(
        "{\"type\":\"compaction\",\"id\":\"c\",\"parentId\":null,\"summary\":\"checkpoint\"}\n",
        "{\"type\":\"message\",\"id\":\"m\",\"parentId\":\"c\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"한글🙂\"}]}}\n",
        "{\"type\":\"thinking_level_change\",\"id\":\"t\",\"parentId\":\"m\"}\n"
    ));
    let pi = Reader::default().read("pi", Some(&reference), 4, 8);
    assert_eq!(pi.messages.last().unwrap().text, "한");
    assert_eq!(pi.checkpoint.as_ref().unwrap().text, "chec");
    assert!(pi.truncated);
    let codex = fixture.reference(concat!(
        "{\"type\":\"compacted\",\"payload\":{\"message\":\"resume checkpoint\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"duplicate\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"actual request\"}]}}\n"
    ));
    let codex = Reader::default().read("codex", Some(&codex), 4, 1024);
    assert_eq!(
        codex
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["actual request"]
    );
}

#[test]
fn unreadable_unsafe_partial_and_legacy_history_is_explicit() {
    let fixture = Fixture::new();
    let legacy: AgentSessionRef =
        serde_json::from_str(r#"{"kind":"id","value":"session"}"#).unwrap();
    assert!(legacy.transcript_path.is_none());
    assert_eq!(
        Reader::default()
            .read("unknown", Some(&legacy), 4, 100)
            .status,
        "unsupported_provider"
    );
    assert_eq!(
        Reader::default().read("claude", None, 4, 100).status,
        "identity_unavailable"
    );
    let reference = fixture.reference(
        "{\"type\":\"user\",\"uuid\":\"u\",\"message\":{\"content\":\"committed\"}}\n{\"type\":",
    );
    let memory = Reader::default().read("claude", Some(&reference), 4, 100);
    assert_eq!(memory.messages[0].text, "committed");
    assert!(memory.truncated);
    #[cfg(unix)]
    {
        let link = fixture.0.join("link.jsonl");
        std::os::unix::fs::symlink(reference.transcript_path.as_ref().unwrap(), &link).unwrap();
        let linked = legacy
            .with_transcript_path(link.to_str().unwrap().into())
            .unwrap();
        assert_eq!(
            Reader::default()
                .read("claude", Some(&linked), 4, 100)
                .status,
            "history_unreadable"
        );
    }
    fs::remove_file(reference.transcript_path.as_ref().unwrap()).unwrap();
    assert_eq!(
        Reader::default()
            .read("claude", Some(&reference), 4, 100)
            .status,
        "history_unreadable"
    );
}
