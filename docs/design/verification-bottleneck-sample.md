# Hand-read samples for verification-bottleneck.md

## Script-only commits (SF `--commits --rows commits`, the non-timing rows, every 5th row, 35 rows)

| Hand class | n | commits |
|---|---|---|
| adapt to an intended UI change | 15 | f5e54a7a cc74f6d3 746234f1 fedb904b ffb95959 be076b7f b619cb2d 062aa59f 8dfbb6a9 b8a44018 78736a75 de6d3af5 5b596381 719cb70a b06baabf |
| timing (a read or a wait) | 6 | a6629256 ae2e2ca2 28b5a25b 7b5e7295 1430c232 eac3b64f |
| the check's own logic (order, fixture, a wrong read) | 13 | 10d18699 492729e2 5f5903a5 91546f82 9960d997 8f502bb9 c77fdc9d 0ecb81b6 464d8c2e 0fa2fe91 7b7e044b 25eb3363 3d263014 |
| refactor | 1 | 0f2dcafc |

Rule-timing rows, every 7th of 71 (10 rows): 8 are timing, and 2 are not (29cb5ebe is a fixture file name, 51223e9a is a fold read).

## Failures fixed in product code only (SF `--yield --rows yield`, 21 runs)

Each row is judged by the worker's turn summary that read the failure.

| Judgment | Runs (task, date) |
|---|---|
| product bug in the task's own change | #275 11-07 (one column, 44 px send button); #279 11-07 (quote tap); #356 11-10 (wide pill labels); #413 11-12 (tables under list items); #420 11-13 x2 ("qa" not refused) |
| design rule (CSS literals, design lint) | #264 11-07; #273 11-07; #415 11-12 |
| not the task's: the worker named it outside its change, and other work rode along | #272, #279 (Jump to latest); #264, #356 (To check motion); #325 (unchanged stamp); #418 (since line); #356 (row moved the chat); #319 (88 px composer, the worker judged it a test bug); #420 (Running pill, the run was cancelled) |
| infra (no assert) | #273, #322, #357 |
