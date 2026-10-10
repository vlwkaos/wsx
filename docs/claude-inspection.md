# Source-only hook inspection (0.30.1-inspect.2)

This branch diagnoses Claude and Codex reports. It is not a stable release or a confirmed hook, prompt-submission, or transcript-saving fix. It preserves daemon revision 20, report-once behavior, generation fences, default hook output, and transcript settings. Keep existing 0.29.3 TUI/daemon/agent sessions running. Do not launch the preview TUI or overwrite installed binaries.

## Run after pulling `feat/claude-inspection`

From the inspection repository root, paste:

```sh
(
  set -e
  cargo build --locked -p wsx -p wsx-daemon
  ./target/debug/wsx agent install claude
  ./target/debug/wsx agent install codex
  claude_root="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
  codex_root="${CODEX_HOME:-$HOME/.codex}"
  touch "$claude_root/hooks/wsx-inspect-enabled" "$codex_root/wsx-inspect-enabled"
  chmod 600 "$claude_root/hooks/wsx-inspect-enabled" "$codex_root/wsx-inspect-enabled"
  ./target/debug/wsx --version
)
```

Expect `wsx 0.30.1-inspect.2`. If inspecting only Claude, omit the Codex install and marker. Use the same absolute custom provider roots as the affected sessions. Existing Rust/Zig and Python 3 requirements apply; no new dependency is added. macOS/Linux only. Codex requires 0.150 or newer. Review changed WSX hook definitions through Codex's native trust flow; enabling a hook does not grant trust.

Let normal work produce a hook event. Do not restart an agent, replay a draft, run the hook manually, inject shell exports into an agent editor, or force persistence. Installing files does not prove that an old session loaded updated hook bindings. A marker enables all sessions invoking that hook directory without modifying their environment.

## Paste the records

After Claude or Codex finishes, run:

```sh
python3 "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/hooks/wsx-agent-inspect.py" --read
python3 "${CODEX_HOME:-$HOME/.codex}/wsx-agent-inspect.py" --read
```

The reader prints only validated `wsx inspect {...}` metadata from each private journal. Skip the other provider's command if it was not installed. Share this output, whether the session predated the update, and whether Claude still shows the transcript warning. No transcript, full debug log, environment dump, or screen capture is needed.

Claude can hide successful hooks' stderr. Unlike inspect.1, inspect.2 saves both successes and failures to `wsx-inspect.jsonl` beside the hook. The journal is mode 600, retains at most 32 records and 64 KiB, and each record is below 2 KiB. Nonblocking locks avoid concurrent record corruption; a busy/unsafe/unavailable journal can drop a record but never change reporting success or failure. The reader refuses symlinks, non-regular files, extra hardlinks, wrong ownership, and nonprivate permissions. Invalid records are not echoed. No log is created while inspection is disabled.

A record proves that invocation reached Claude integration 21 or Codex integration 15. No record means the event/new binding/inspection did not produce a usable journal record; it does not prove that reporting succeeded. Journal failure can still emit bounded stderr metadata with `inspection_log_unavailable`. Stop and share the reader's fixed error rather than relaxing file permissions.

## What records establish

| Field | Meaning and limit |
|---|---|
| `provider`, `recorded_at_unix_ms`, `action`, `pane` | Correlation metadata for this invocation, not conversation content. |
| `hook_build`, `hook_integration` | Version embedded in the hook that ran, not the agent's launch version. |
| `reporter_version_observed` | Protected reporter's current version after reporting, not proof of the executable used before a handoff. |
| `daemon_version_observed`, `daemon_revision_observed` | Read-only observation after reporting. Only source-verified 0.29.3, released 0.30.0, and this build may probe runtime status; other reporters leave it unknown. |
| `runtime_generation` | Presence only. `stale_runtime` establishes rejection; presence alone is not validity. |
| `report_attempted`, `report_exit`, `error_code` | CLI reporting result or skipped-input reason. Exit zero is a report ACK, not prompt acceptance or native completion. |
| `warnings` | Older/different/unknown reporter, missing/rejected generation, or unavailable journal. Older does not imply incompatible. |
| Child/persistence markers | Hook-process presence only, not startup environment or actual transcript saving. |
| `agent_launch_version`, `agent_startup_environment` | Deliberately unknown. Existing processes do not inherit frontend environment changes. |

No prompt, transcript contents, native session ID, generation value, credential, screen text, keystroke, or path is recorded. At most two protected read-only probes cap output/time (128/8192 bytes and 0.75 seconds each). Reporter stderr contributes only a bounded error code; it is never echoed wholesale. Failed inspection never replays a report. Heartbeats are unchanged.

## Fresh sessions and disable

For a separately created managed pane, `WSX_INSPECT=1 claude` or `WSX_INSPECT=1 codex` enables inspection at launch. Do not restart an existing session for this test. v0.30.0 strips inherited `CLAUDE_CODE_CHILD_SESSION` at fresh PTY spawn; an existing 0.29.3 daemon does not gain that repair by installing hooks. No transcript-saving claim follows from these diagnostics.

Remove only markers you created to stop marker-enabled recording:

```sh
rm -- "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/hooks/wsx-inspect-enabled"
rm -- "${CODEX_HOME:-$HOME/.codex}/wsx-inspect-enabled"
```

An inherited `WSX_INSPECT=1` persists until that process naturally exits. Existing private journals remain for diagnosis; removing a marker does not delete them or clear environment flags. Restore published hooks later with the installed stable CLI's `wsx agent install <provider>` when safe. No stable tag, release publication, or live-session replacement belongs to this inspection flow.
