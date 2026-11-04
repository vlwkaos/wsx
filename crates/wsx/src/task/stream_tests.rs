//! The adapters' own read loops over streams recorded from the real CLIs
//! (`crates/wsx/tests/fixtures/agent-streams`, made by `scripts/record-agent-streams.py`): what
//! a turn's `TurnResult` holds is judged against what the CLI really sent, not against a fake's
//! idea of it. ref: docs/tasks.md "Agent stream recordings"

use super::agent::{AgentLine, TurnResult};
use wsx_core::task::manager::says_nothing;

fn recording(adapter: &str, name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/agent-streams/{adapter}/{name}.jsonl",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The lines the host would get, and the turn's one result.
fn run(adapter: &str, name: &str, cost_base: u64) -> (Vec<AgentLine>, TurnResult) {
    let stream = recording(adapter, name);
    let mut lines = Vec::new();
    let keep = |line| {
        lines.push(line);
        true
    };
    match adapter {
        "claude" => super::claude::read_stream(stream.as_slice(), cost_base, keep),
        _ => super::pi::read_stream(stream.as_slice(), cost_base, keep),
    }
    let mut results = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| matches!(line, AgentLine::Result(_)));
    let (at, _) = results.next().expect("the recording has a result");
    assert!(results.next().is_none(), "one turn, one result");
    assert_eq!(at, lines.len() - 1, "the result ends the turn");
    let Some(AgentLine::Result(result)) = lines.pop() else {
        unreachable!()
    };
    (lines, *result)
}

/// Token counts and cost: input, output, cache read, cache write, micro USD.
fn counts(result: &TurnResult) -> (u64, u64, u64, u64, u64) {
    let usage = &result.usage;
    (
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_creation_tokens,
        usage.cost_micro_usd,
    )
}

#[test]
fn claude_plain_turn() {
    let (lines, result) = run("claude", "plain", 0);
    assert!(matches!(lines.first(), Some(AgentLine::Init(id)) if !id.is_empty()));
    assert!(lines
        .iter()
        .any(|line| matches!(line, AgentLine::Progress { said: true, text } if text == "Hello from the fixture.")));
    assert!(!result.is_error && result.subtype == "success" && result.limit.is_none());
    assert_eq!(result.text, "Hello from the fixture.");
    assert_eq!(counts(&result), (2, 11, 11384, 7198, 1559));
    assert_eq!(result.usage.session_cost_micro_usd, 1559);
    assert_eq!(result.usage.context_tokens, 2 + 11384 + 7198);
    assert_eq!(result.usage.model, "claude-haiku-5-5");
    assert_eq!((result.usage.calls, result.usage.duration_ms), (1, 1653));
    assert!(!result.answers_nothing());
}

#[test]
fn claude_keeps_every_text_block_of_a_turn() {
    // ^ The result event holds only the last block ("two"); the turn said "one", used Bash, then "two".
    let (_, result) = run("claude", "text-tool-text", 0);
    assert_eq!(result.text, "one\n\ntwo");
    assert_eq!(counts(&result), (4, 77, 29989, 9346, 2208));
    // ^ Two calls: the text and tool lines of the first share a message id.
    assert_eq!(result.usage.calls, 2);
    assert_eq!(result.usage.context_tokens, 2 + 18605 + 2125);
}

#[test]
fn claude_cost_counts_from_the_sessions_earlier_spend() {
    let (_, result) = run("claude", "text-tool-text", 1000);
    assert_eq!(result.usage.cost_micro_usd, 1208);
    assert_eq!(result.usage.session_cost_micro_usd, 2208);
}

#[test]
fn claude_quiet_turn_has_no_words() {
    let (_, result) = run("claude", "quiet", 0);
    assert!(!result.is_error);
    assert!(says_nothing(&result.text), "{:?}", result.text);
    assert_eq!(result.text, "");
}

#[test]
fn claude_subagent_words_are_no_reply() {
    // ^ The subagent said "ok" (parent_tool_use_id set), and its model call is another context.
    let (_, result) = run("claude", "subagent", 0);
    assert_eq!(
        result.text,
        "The subagent is still running. I'll say \"finished\" when its result comes back."
    );
    assert_eq!(result.usage.calls, 2);
    assert_eq!(result.usage.context_tokens, 2 + 18599 + 2538);
    assert_eq!(result.background, 0);
}

#[test]
fn claude_worker_result_is_the_structured_output() {
    // ^ A worker turn: `--json-schema` makes the model call StructuredOutput; the result carries it.
    let (_, result) = run("claude", "structured", 0);
    assert!(!result.is_error);
    assert_eq!(
        result.structured,
        Some(serde_json::json!({ "status": "done" }))
    );
    assert_eq!(counts(&result), (2, 197, 0, 18677, 3834));
}

#[test]
fn claude_error_result_is_an_error_without_words() {
    let (_, result) = run("claude", "error", 0);
    assert!(result.is_error && result.limit.is_none());
    assert_eq!(result.subtype, "error_max_turns");
    assert_eq!(result.text, "");
    assert_eq!(counts(&result), (2, 77, 11384, 7201, 1593));
    assert!(!result.answers_nothing());
}

#[test]
fn pi_plain_turn() {
    let (lines, result) = run("pi", "plain", 0);
    assert!(lines
        .iter()
        .any(|line| matches!(line, AgentLine::Progress { said: true, .. })));
    assert!(!result.is_error && result.subtype == "success" && result.limit.is_none());
    assert_eq!(result.text, "Hello from the fixture.");
    assert_eq!(counts(&result), (1274, 9, 0, 0, 266));
    assert_eq!(result.usage.context_tokens, 1274);
    assert_eq!(result.usage.model, "gpt-5.6-luna");
    assert_eq!(result.usage.calls, 1);
}

#[test]
fn pi_keeps_every_text_message_of_a_run() {
    // ^ Pi ends the first message "one" with a tool call and the second "two": two message_end events.
    let (lines, result) = run("pi", "text-tool-text", 0);
    assert_eq!(result.text, "one\n\ntwo");
    assert_eq!(counts(&result), (1478, 36, 1152, 0, 362));
    assert_eq!(result.usage.calls, 2);
    assert_eq!(result.usage.context_tokens, 185 + 1152);
    assert!(lines
        .iter()
        .any(|line| matches!(line, AgentLine::Progress { said: false, text } if text.starts_with("bash echo done"))));
}

#[test]
fn pi_quiet_turn_has_no_words() {
    let (_, result) = run("pi", "quiet", 0);
    assert!(!result.is_error);
    assert!(says_nothing(&result.text), "{:?}", result.text);
}

#[test]
fn pi_provider_refusal_is_an_error_with_a_limit() {
    let (_, result) = run("pi", "error", 0);
    assert!(result.is_error && result.subtype == "error");
    assert!(
        result.text.contains("credit balance is too low"),
        "{}",
        result.text
    );
    // ^ No credit will not return soon: the account counts as limited for hours.
    assert!(result.limit.is_some_and(|limit| limit.resets_at.is_some()));
}
