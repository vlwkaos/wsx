# Terminal synchronized output

Programs such as Claude Code, Pi and Codex wrap each screen update in synchronized output (`CSI ? 2026 h` ... `CSI ? 2026 l`). While the mode is set, wsxd parses PTY output but sends no frames, so a client never shows a half-drawn screen.

## Expiry

A program that is ended inside an update, for example by Ctrl+C, never resets the mode. The shell that returns does not reset it either. Without a bound, the session shows the last sent frame (often blank) and hides all later output, although the shell still receives input.

`Emulator::synchronized_output` in `crates/wsx-terminal/src/lib.rs` owns the bound. The PTY reader and `presentation_sample` both call it. When the mode has stayed set for `SYNCHRONIZED_OUTPUT_TIMEOUT` (one second, the Ghostty value), the emulator resets the mode and frames resume. A program that sets and releases the mode normally is not affected.

A terminal resize also clears the mode inside libghostty-vt. That is why a resize appeared to repair a stuck session.

## Verification

`scripts/runtime-smoke.py` starts a real PTY session, subscribes, then lets the child print `CSI ? 2026 h` followed by text and never release the mode. The stream must deliver the text within three seconds. Before the repair the frame stayed blank.
