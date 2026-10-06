# Terminal exit ordering

On Unix, the PTY reader owns natural terminal exit. It consumes the child's final output, including bounded clipboard effects, before publishing reader EOF or error. The child waiter owns reaping and process-group cleanup, not natural stream exit. Waiter errors still notify consumers. Explicit termination keeps its existing immediate exit boundary.

The terminal stream samples the queued effects before emitting `Exited`. Publishing exit from the child waiter can bypass this ordering because child reaping and PTY draining run on different threads. No sleep or presentation delay can establish the required ordering.

Daemon revision 18 records this runtime-owned repair. Protocol fields and presentation cadence are unchanged. Non-Unix waiter behavior is preserved; this repair does not establish Windows execution.

## Verification

- `tests::natural_exit_waits_for_final_pty_effects_to_drain` joins the production waiter before supplying final VT bytes. It verifies that reaping alone cannot publish exit and that final effects remain available when reader EOF publishes exit.
- `scripts/runtime-smoke.py` exercises an actual Unix PTY and persistent terminal stream. A child emits OSC 52 and immediately exits; the client must receive `clipboard_write` before `exited`. The Linux CI failure reproduced the reversed boundary. The assertion is not relaxed.
- Terminal unit tests also cover FIFO bounds, synchronization, selection, process-group ownership and live handoff. Exact-commit Linux/macOS CI remains the release gate.
