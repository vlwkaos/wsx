# Terminal synchronized output

Programs such as Claude Code, Pi and Codex wrap each screen update in synchronized output (`CSI ? 2026 h` ... `CSI ? 2026 l`). While the mode is set, wsxd parses PTY output but sends no frames, so a client never shows a half-drawn screen.

## Raw stream

The raw stream (`docs/terminal-raw-stream.md`) sends the program's own bytes to a client emulator. xterm.js 5.5 ignores mode 2026 (it came in 6.0, xterm.js PR #5453), so it drew each half of Claude's input box as it came, which read as flicker on the arrow keys. While an update is open, `stream_raw_terminal_output` sends the bytes before its `CSI ? 2026 h` and holds the rest; when the update ends, or its bound expires, the whole update goes in one `output` message. A held update is checked again every 50 ms.

## Expiry

A program that is ended inside an update, for example by Ctrl+C, never resets the mode. The shell that returns does not reset it either. Without a bound, the session shows the last sent frame (often blank) and hides all later output, although the shell still receives input.

`Emulator::synchronized_output` in `crates/wsx-terminal/src/lib.rs` owns the bound. The PTY reader and `presentation_sample` both call it. When the mode has stayed set for `SYNCHRONIZED_OUTPUT_TIMEOUT` (one second, the Ghostty value), the emulator resets the mode and frames resume. A program that sets and releases the mode normally is not affected.

A terminal resize also clears the mode inside libghostty-vt. That is why a resize appeared to repair a stuck session.

## Verification

`scripts/runtime-smoke.py` starts a real PTY session, subscribes, then lets the child print `CSI ? 2026 h` followed by text and never release the mode. The stream must deliver the text within three seconds. Before the repair the frame stayed blank. On the raw stream it checks that the bytes before an open update arrive at once, that the update arrives only after it ends and in one message, and that an update that never ends arrives after its bound.
