# Terminal exit ordering

On Unix, the PTY reader owns natural terminal exit. It consumes the child's final output, including bounded clipboard effects, before publishing reader EOF or error. The child waiter owns reaping and process-group cleanup, not natural stream exit. Waiter errors still notify consumers. Explicit termination keeps its existing immediate exit boundary.

The terminal stream samples the queued effects before emitting `Exited`. Publishing exit from the child waiter can bypass this ordering because child reaping and PTY draining run on different threads. No sleep or presentation delay can establish the required ordering.

Daemon revision 18 records this runtime-owned repair. Protocol fields and presentation cadence are unchanged. Non-Unix waiter behavior is preserved; this repair does not establish Windows execution.

## The PTY after a natural exit

When the reader sees EOF or EIO (every slave closed, final output read) it publishes the exit, then closes the writer and the master (`release_pty`). An exited pane keeps its screen and its saved recipe, not a kernel PTY: a machine has `kern.tty.ptmx_max` masters (macOS 511) for all processes, and `openpty` fails with ENXIO ("Device not configured") when they run out. Explicit `terminate` and other reader errors keep the master until the runtime drops. A live handoff already sends an exited pane without a PTY.

## In the TUI

When the open pane's program exits (an agent's `/exit`, Ctrl-D in a shell), the TUI leaves Terminal mode, marks the pane exited at once (`TerminalServerMessage::Exited` comes before the next snapshot) and says "Terminal process exited. Enter starts it again; d closes the session". A snapshot that first reports the pane `exited` (the reattach after a lost stream finds it, or no stream is open) gives the same mode change and line through the same function, `terminal_program_exited`. The row reads "· exited". Enter on an exited pane sends `SessionRestart` with the pane's current revision and opens it: the saved command runs again in the same pane, and an agent with a saved conversation resumes it. Before, Enter attached to the dead pane, got `Exited` again and showed an empty screen that took no keys.

Two TUI facts make the restart safe. The exited pane's frame is dropped at once (`TerminalSurfaces::forget`): the restarted runtime keeps the terminal identity but starts at a lower frame revision, and the old frame would hide the new ones. A snapshot older than a revision the window has applied or caused (the restart's `Ack`) is not applied: a refresh read before the restart still says exited and would end the new stream.

## Verification

- `tests::natural_exit_waits_for_final_pty_effects_to_drain` joins the production waiter before supplying final VT bytes. It verifies that reaping alone cannot publish exit and that final effects remain available when reader EOF publishes exit.
- `scripts/runtime-smoke.py` exercises an actual Unix PTY and persistent terminal stream. A child emits OSC 52 and immediately exits; the client must receive `clipboard_write` before `exited`. The Linux CI failure reproduced the reversed boundary. The assertion is not relaxed.
- `scripts/runtime-smoke.py` keeps a real TUI in Terminal mode on a shell, types `exit`, waits for the notice, presses Enter and reads the restarted shell's answer to a typed line.
- `wsx-daemon::live_handoff ended_and_closed_sessions_give_their_pty_back_also_through_a_handoff` counts the daemon's `/dev/ptmx` descriptors: ended sessions, a handoff and closed sessions leave none.
- Terminal unit tests also cover FIFO bounds, synchronization, selection, process-group ownership and live handoff. Exact-commit Linux/macOS CI remains the release gate.
