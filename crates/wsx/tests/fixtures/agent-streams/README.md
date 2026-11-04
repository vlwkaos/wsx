# Agent stream recordings

Real output of the headless agent CLIs that the task adapters read, one turn per file.
Made by `python3 -B scripts/record-agent-streams.py` (docs/tasks.md "Agent stream recordings"); ids, paths and rate-limit numbers are scrubbed, field names, types and order are not.
Used by `crates/wsx/src/task/stream_tests.rs` (the adapters' parse path) and `crates/wsx/tests/agent_stream_fakes.rs` (the fakes' shapes).

## claude

Recorded with claude 2.1.316 (Claude Code): `claude -p --input-format stream-json --output-format stream-json --verbose --model haiku`

- `plain.jsonl`: a plain text turn
- `text-tool-text.jsonl`: text, a Bash tool use, then more text (the result holds only the last block)
- `quiet.jsonl`: a quiet turn: the whole reply is -
- `subagent.jsonl`: a subagent: its lines carry parent_tool_use_id
- `structured.jsonl`: a worker turn: --json-schema gives the result a structured_output
- `error.jsonl`: a turn that ends in an error result (--max-turns 1 with a tool request)

## pi

Recorded with pi 1.4.0: `pi --mode rpc --model openai-codex/gpt-5.6-luna --thinking off --no-session`

- `plain.jsonl`: a plain text turn
- `text-tool-text.jsonl`: text, a bash tool call, then more text in one run
- `quiet.jsonl`: a quiet turn: the whole reply is -
- `error.jsonl`: a provider refusal (an Anthropic account without credit)
- No subagent recording: Pi has no subagent of its own.
