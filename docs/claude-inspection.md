# Source-only Claude inspection (0.30.1-inspect.1)

This branch is a diagnostic build based on v0.30.0, not a stable release or a confirmed Claude fix. It does not change daemon revision 20, prompt submission, transcript persistence, report authority, or default hook output. Do not launch the preview TUI or stop the daemon just to inspect an existing session.

## Build on the affected machine

Use a separate checkout. Keep installed binaries and running sessions intact. The existing Rust/Zig build requirements apply; no new dependency is added. Inspection supports macOS and Linux, not Windows.

```sh
git clone --branch feat/claude-inspection --single-branch https://github.com/vlwkaos/wsx.git wsx-inspection
cd wsx-inspection
cargo build --locked -p wsx -p wsx-daemon
./target/debug/wsx --version
./target/debug/wsx agent install claude
```

Expect `wsx 0.30.1-inspect.1`. Installation refreshes only the Claude assets and WSX-owned configuration bindings through the existing installer. Claude integration is 20. Do not run `cargo install`, overwrite Homebrew binaries, or create a release tag.

## Existing session (without restart or prompt replay)

An old session can retain an old reporter path, missing generation metadata, or loaded hook configuration. Installing files is not proof that the session loaded them. Do not inject `export` commands into an active Claude editor.

Enable the empty, owner-only marker beside the installed hook. Use an absolute `CLAUDE_CONFIG_DIR` if configured:

```sh
root="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
touch "$root/hooks/wsx-inspect-enabled"
chmod 600 "$root/hooks/wsx-inspect-enabled"
```

The marker enables inspection for **all Claude sessions that invoke this hook directory**, including existing sessions without `WSX_INSPECT` in their environment. Symlink, nonempty, wrong-owner, or nonprivate markers do not enable it. It does not load or restart an old session's hook configuration. Let normal work produce the next lifecycle/Stop event; do not resend an uncertain prompt or send speculative Enter.

A `wsx inspect {…}` line proves that invocation reached integration 20. No line can also mean no event, an old hook binding, absent Python, or failed inspection; it does not prove successful reporting. Use Claude's supported reload only when safe, or a separately created test session. Leave protected existing sessions intact.

## Fresh test session

In a new managed pane, inspect marker presence **before** launching Claude. Do not print values or dump the environment:

```sh
if [ "${CLAUDE_CODE_CHILD_SESSION+x}" = x ]; then printf 'launch child_session_marker=present\n'; else printf 'launch child_session_marker=absent\n'; fi
if [ "${CLAUDE_CODE_FORCE_SESSION_PERSISTENCE+x}" = x ]; then printf 'launch force_persistence_marker=present\n'; else printf 'launch force_persistence_marker=absent\n'; fi
WSX_INSPECT=1 claude
```

The flag is launch-time only. Setting it on a wsx frontend cannot change an existing Claude environment. v0.30.0 already strips inherited `CLAUDE_CODE_CHILD_SESSION` at fresh PTY spawn, before explicit pane overrides. Hook-process marker presence is not evidence of Claude's startup environment or transcript saving. Do not force persistence as part of this inspection.

## Read the result

| Field | Meaning and limit |
|---|---|
| `hook_build`, `hook_integration` | Build embedded in the hook that actually ran. Not the agent's launch version. |
| `reporter_version_observed` | Protected reporter's current `--version`, observed after the report. A stable symlink can change during handoff; this is not proof of the executable used by an earlier report. |
| `daemon_version_observed`, `daemon_revision_observed` | Read-only `runtime status` observation after the report. Probed only through released 0.30.0 or this inspection build, whose non-starting dispatch is source-backed. Other reporters leave it unknown; no eager bootstrap is assumed safe. |
| `runtime_generation` | Presence only. `stale_runtime` proves the report was rejected by its generation fence; presence alone does not prove validity. |
| `report_attempted`, `report_exit`, `error_code` | Actual report attempt/CLI exit and bounded error code, or a skipped-input reason. Exit zero is a report ACK, not prompt acceptance or native completion. |
| `warnings` | Older/different/unknown reporter or missing/rejected generation. Older does not automatically mean incompatible or a broken session. |
| `agent_launch_version`, `agent_startup_environment` | Deliberately unknown. Current package or daemon versions do not establish how an existing agent launched. |

Only safe metadata is emitted. No prompt, transcript contents, native session ID, generation value, credential, screen text, keystroke, or path is logged. Each line is below 2 KiB. At most two read-only CLI probes cap output/time (128/8192 bytes and 0.75 seconds each); failed inspection does not change the report result. The report itself is never replayed. Heartbeat behavior is unchanged.

For a remote receipt, share the version output, inspection lines, whether the session predated the update, and whether the transcript warning occurred in an existing or fresh session. Do not send a transcript or terminal dump. Actual Claude Stop-hook operation, prompt submission, and transcript saving remain separate remote verification outcomes.

## Disable

If you created the marker above, remove only that marker:

```sh
rm -- "$root/hooks/wsx-inspect-enabled"
```

For environment-enabled sessions, the flag remains in that process until it exits naturally. Disabling the marker cannot remove an inherited environment flag. To restore published hooks later, run the installed stable CLI's `wsx agent install claude` when safe; changing files alone still does not prove loaded configuration changed.
