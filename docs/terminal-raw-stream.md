# Raw terminal stream

`wsxd` gives a client that runs its own terminal emulator the bytes the pane's program wrote. The hosted web client uses it for xterm.js (`docs/web-client.md`). The TUI keeps the frame stream (`terminal_subscribe`), which is unchanged.

Protocol 17, daemon revision 23, capability `raw_terminal_stream`.

## Request

`terminal_raw_subscribe` upgrades a connection after `hello`, like `terminal_subscribe`:

```json
{"method":"terminal_raw_subscribe","params":{"pane_id":7,"client_id":42,"takeover":false,"rows":40,"cols":120}}
```

- It takes the pane's one writable lease. Without `takeover` it fails with `terminal_busy` while another live client holds the lease. A new subscribe with the same `client_id` replaces that client's own stream.
- `rows` and `cols` are applied before the `ack`. Both 0 keep the pane's size.
- The client sends `TerminalClientMessage` lines: `input`, `paste`, `resize`, `heartbeat` (every second), `resync`, `detach`.

## Server messages

| Message | Meaning |
|---|---|
| `replay {ansi, rows, cols, truncated}` | Reset the client emulator, set this size, write `ansi`. Output continues at the next byte. |
| `output [bytes]` | PTY bytes, at most 256 KiB per message. |
| `resized {rows, cols}` | The pane size changed. A `replay` always follows. |
| `clipboard_write [bytes]` | The program set the clipboard (OSC 52). |
| `error {code, message}` | The stream ended: `lease_transferred`, `lease_lost`, `daemon_handoff`, or an input error. |
| `exited` | The program exited. Every output byte was sent before it. |

## Rules

1. **No gap and no repeat.** `Emulator::ingest` (`crates/wsx-terminal/src/lib.rs`) counts every PTY byte and, from the first replay on, keeps the last 1 MiB in a ring. `raw_replay` reads the screen and the byte count under one emulator lock, so the replay plus the bytes after it are exactly the pane's output.
2. **A slow client gets a replay, not a gap.** When the next byte is no longer in the ring, the stream sends a new `replay`. A client that does not read for 5 seconds loses the connection.
3. **A resize restarts from a replay.** Two emulators reflow a resized screen differently. The byte stream cannot keep them equal, so `wsxd`'s screen is sent again after each size change.
4. **The replay is the handoff screen.** It uses `bounded_handoff_ansi` for at most 512 KiB of scrollback, scrolls exactly those lines off the client screen, writes the screen rows from row 1, then the modes that differ from a reset emulator (cursor keys, wrap, cursor visibility, mouse tracking and encoding, focus events, bracketed paste) and the cursor position. With the alternate screen active it holds that screen only.
5. **Messages are written outside the pane operation lock.** A client that stops reading cannot delay a takeover.
6. **Live handoff.** The stream ends with `daemon_handoff`. The successor's tap starts empty; the client subscribes again and gets a replay.

## The client must not answer queries

`wsxd`'s emulator answers device and color queries. A client emulator that answers them too sends a second reply into the PTY. The web client disables those replies in xterm.js.

## Not covered

- The kitty keyboard protocol state is not in the replay. xterm.js 5.5 does not implement it; input uses legacy key encoding.
- While the alternate screen is active, a replay does not carry the primary screen or its scrollback.
- The pen (current SGR), scroll region and character sets are not in the replay. A full-screen program sets them again when it redraws.

## Verification

- `scripts/runtime-smoke.py`: replay then exact bytes, resize with replay, refusal without takeover, takeover, keep-size subscribe, slow subscriber, `resync`, exit ordering, and the terminal latency gate.
- `crates/wsx-daemon/tests/live_handoff.rs`: a raw stream ends with `daemon_handoff`, and the successor replays the restored screen and keeps the PTY.
