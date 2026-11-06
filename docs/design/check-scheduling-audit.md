# Check scheduling: where a task's check time goes, and what makes it shorter

Audit of 2026-11-07. Contract: `docs/tasks.md` "Check slot" and "Scenarios"; code `crates/wsx/src/task/slot.rs`, `scripts/ci-scenario.py`, `.github/workflows/ci.yml`. Earlier research: `check-queue-research.md` (11-04).
Window A: 48 h before 2026-11-06 18:43 KST, when `web` and `task` became `ci:`. Window B: after that, 11.7 h.

## Answer

1. 70% of check time is queue (4907 of 6952 min, Q2 plus Q3), and 91% of the queue is scenario runs waiting for their lane (Q3). Builds are not the cost (Q10).
2. The `web` scenario fails 94 of 127 runs, and 61% of its run-minutes are failed runs (Q4). 73 of 94 failures are in the tasks half of the journey (Q5).
3. Window B cannot show the CI gain yet. The CI jobs reached origin only with the 11-06 21:24 release. Since then 4 of 5 `ci:` runs stopped in about 1 s, and 1 failed after 3.9 min (Q9).

## Measurements (window A unless named)

| # | Measure | Number |
|---|---|---|
| Q1 | per task, 72 tasks: queue / run | p50 25.8 / 11.3 min, p90 204.3 / 73.1 min |
| Q2 | plain runs (fmt, build, nextest) | 209 runs, 7 failed; queue p50 0, p90 8.3 min, sum 449 min; run p50 1.7 min |
| Q3 | scenario runs | 141 runs, 101 failed; queue p50 29.3, p90 62.1, sum 4458 min; run p50 12.2 min, sum 1680 min |
| Q4 | `web` runs | 127, 94 failed, 1525 run-min, of it 936 in failed runs; a failed run's p50 is 9.2 min |
| Q5 | where `web` fails | `task_journey` 73, `web_journey` 18, other 3 |
| Q6 | a passing `web` run | whole p50 1069 s; its tasks half (`--part tasks`) p50 695 s, so chat half plus build is about 374 s |
| Q7 | retries | 31 tasks ran `web`: p50 4 runs a task, max 11; 28 of 31 had a failure |
| Q8 | failures whose assert also failed in another task | 49 runs, 547 run-min (36% of Q4). "long chat render ... CPU 4x" 18 runs in 10 tasks; "the manager answered the lands of #7 and #8" 13 in 7; "after Back the page is" 7 in 5 |
| Q9 | after the cut | `ci:` runs 5, all failed: 4 refused in 1.1-1.5 s, 1 ran 233.7 s; plain 32 passed, 1 failed |
| Q10 | `cargo build -p wsx -p wsx-daemon` alone | 224 runs, p50 16 s, p90 46 s, max 163 s, 81 min in all; "Blocking waiting for file lock" in 3 outputs |
| Q11 | CI (gh, 5 runs) | job start wait 0.0-0.2 min; task scenario step 24.9-25.8 min, against 16.2 min p50 on the Mac (11-04 doc): CI is about 1.55 times slower |

Q1-Q10 come from one query over `~/.local/state/wsx/tasks/wsx-8cbe3f08/*/log.jsonl` (`checks` and `check_run` events, results with `queued_ms`, `stored` excluded). Save it as `q.py`, run `python3 -I q.py`:

```python
import glob,json,re,collections as C
S='/Users/vlwkaos/.local/state/wsx/tasks/wsx-8cbe3f08'; CUT=1793958227000; LO=CUT-48*3600000  # 11-06 18:43 KST
kind=lambda c:next((n for k,n in (('ci:','ci'),('web-client','web'),('test-task','task'),('smoke','smoke'),('boundary','boundary')) if k in c),'plain')
R,E,T=[],[],C.defaultdict(lambda:[0,0,0,0])  # R: one row per command, E: per run, T: task -> queue, run, web runs, web fails
for f in glob.glob(S+'/[0-9]*/log.jsonl'):
  t=int(f.split('/')[-2])
  for l in open(f,errors='replace'):
    try: e=json.loads(l)
    except ValueError: continue
    rs=[r for r in e.get('results',[]) if 'queued_ms' in r and not r.get('stored')] if e.get('kind') in ('checks','check_run') else []
    if not rs: continue
    R+=[dict(r,t=t,at=e['unix_ms'],k=kind(r['command'])) for r in rs]
    if LO<=e['unix_ms']<CUT:
      sc=any(kind(r['command'])!='plain' for r in rs); q=rs[0]['queued_ms']; run=sum(r.get('ms',0) for r in rs)
      E.append((sc,q,run,all(r['exit']==0 for r in rs))); T[t][0]+=q; T[t][1]+=run
      w=[r for r in rs if kind(r['command'])=='web']; T[t][2]+=len(w); T[t][3]+=sum(r['exit']!=0 for r in w)
p=lambda v,x:sorted(v)[min(len(v)-1,int(x*len(v)))] if v else 0; m=lambda ms:round(ms/60000,1)
for sc in (False,True):  # Q2, Q3
  e=[x for x in E if x[0]==sc]; print('scenario' if sc else 'plain','runs',len(e),'failed',sum(not x[3] for x in e),'queue p50/p90/sum',m(p([x[1] for x in e],.5)),m(p([x[1] for x in e],.9)),m(sum(x[1] for x in e)),'run p50/p90/sum',m(p([x[2] for x in e],.5)),m(p([x[2] for x in e],.9)),m(sum(x[2] for x in e)))
print('Q1 tasks',len(T),'queue p50/p90',m(p([v[0] for v in T.values()],.5)),m(p([v[0] for v in T.values()],.9)),'run p50/p90',m(p([v[1] for v in T.values()],.5)),m(p([v[1] for v in T.values()],.9)))
W=[v for v in T.values() if v[2]]; print('Q7',len(W),'runs/task p50/max',p([v[2] for v in W],.5),max(v[2] for v in W),'with a failure',sum(v[3]>0 for v in W))
w=[r for r in R if r['k']=='web' and LO<=r['at']<CUT]; bad=[r for r in w if r['exit']]
print('Q4',len(w),'failed',len(bad),'run-min',m(sum(r['ms'] for r in w)),'failed run-min',m(sum(r['ms'] for r in bad)),'failed p50',m(p([r['ms'] for r in bad],.5)))
print('Q5',C.Counter((re.findall(r'line \d+, in (web_journey|task_journey)',r['output']) or ['other'])[-1] for r in bad))
tp=[float(s) for r in w if not r['exit'] for s in re.findall(r'"part": "tasks",\s*"seconds": ([\d.]+)',r['output'])]
print('Q6 tasks part s',p(tp,.5),'whole s',p([r['ms']/1000 for r in w if not r['exit']],.5))
G=C.defaultdict(list)
for r in bad:
  a=re.findall(r'^AssertionError: [^\n]{8,60}',r['output'],re.M)
  if a: G[re.sub(r'\d+','N',a[-1])].append(r)
sh=[r for g in G.values() if len({r['t'] for r in g})>1 for r in g]; print('Q8',len(sh),'run-min',m(sum(r['ms'] for r in sh)))
for k,g in sorted(G.items(),key=lambda i:-len(i[1]))[:4]: print(' ',len(g),'runs',len({r['t'] for r in g}),'tasks:',k[16:70])
b=[r['ms'] for r in R if r['command']=='cargo build -p wsx -p wsx-daemon' and LO<=r['at']<CUT]; print('Q10',len(b),'p50/p90/max s',p(b,.5)//1000,p(b,.9)//1000,max(b)//1000,'min',m(sum(b)))
print('Q10 lock waits',sum('Blocking waiting for file lock' in r['output'] for r in R if LO<=r['at']<CUT))
a=[r for r in R if r['at']>=CUT]; print('Q9',C.Counter((r['k'],r['exit']==0) for r in a),'ci ms',[r['ms'] for r in a if r['k']=='ci'])
```

Q11: `gh run view <id> --repo vlwkaos/wsx --json createdAt,jobs` for runs 42296537824, 42144465668, 42140520946, 42047237009, 42047142160 (job `startedAt` minus run `createdAt`; step `completedAt` minus `startedAt`).

## Options

Gain is per 48 h of window A load, unless named. "Every project" means the change is in wsx's task host or config contract, not in wsx's own test scripts.

| Option | Gain | Cost | Risk | Every project |
|---|---|---|---|---|
| A. Scenario parts: run a scenario's parts as parallel CI jobs and let a worker rerun one part | wall per `web` run about 35% shorter (1069 s to about 695 s plus setup, Q6); a worker fixing a tasks failure (73 of 94, Q5) skips the 374 s chat half | ci.yml matrix on `part`, `ci-scenario.py` waits for all jobs, `--part` exists | 2 of the account's 5 macOS jobs a run; the full run still gates the result | yes, a scenario names its parts |
| B. Main baseline: run each scenario on main after a land; a task failure that main also has is answered "fails on main too" and starts no fix turn | up to 49 runs, 547 run-min (Q8); stops the 7-run regression of #269's report | a CI run per land; host keeps the last main result for each scenario | a new failure with the same assert text is hidden; match by part and assert, show the main run | yes |
| C. One retry on the same tree for a failed `ci:` scenario, the answer says "passed on retry" | the timing asserts (18 runs in 10 tasks, Q8) stop starting worker turns | host retries once, no slot for `ci:` | hides a real flake if the answer does not show it | yes |
| D. Test impact: pick parts from the changed paths | skips the chat half for a tasks-only change | a path map per project, kept by hand | a missed mapping lands a break; the full run after a land must stay | partly |
| E. Two runs in the plain lane when load allows | at most 449 waiter-min (Q2); plain p90 queue 8.3 min | `slot.rs` second lock and a load check | 11-04 doc: a second run overloaded the machine | yes |
| F. Shared cargo target or build cache across worktrees | at most 81 build-min (Q10); 3 lock waits | `CARGO_TARGET_DIR` locks the whole folder, so builds run one at a time; a cross-workspace cache is only a Rust project goal, not stable; sccache is a new dependency (`/sec`) | serialized builds, `cargo clean` hits every task | Rust only |
| G. Batch several ready tasks into one run | none for check-run: each task has its own tree; lands' checks take 1.7 min (Q2) | merge-queue logic with bisection | a failed batch needs bisect runs | yes |

Sources: [Rust project goal "Rework Cargo Build Dir Layout"](https://rust-lang.github.io/rust-project-goals/2025h2/cargo-build-dir-layout.html) (whole build-dir lock today, shared cache planned); [Trunk batching](https://docs.trunk.io/merge-queue/batching), [Graphite batching](https://graphite.com/blog/merge-queue-batching) (batches pay off for high land rates and bisect on failure).

## Ranking and fix tasks

Rank: A, B, C, then D, E, F, G. A and B attack the 1525 `web` run-minutes and their reruns (Q4, Q7). F and G save under 100 min.

1. **Scenario parts (A).** Files: `wsx.config.yml` (`web` with parts `chat`, `tasks`), `scripts/ci-scenario.py` (dispatch one job for each part and join their answers), `.github/workflows/ci.yml` (`web-scenario` matrix on `part`), `docs/tasks.md` "Scenarios". Proof: one `wsx task check-run web` run shows two CI jobs. Its `ms` is below the single-job run of the same commit. A failure names the part that failed.
2. **Main baseline (B), after #269.** Files: `crates/wsx/src/task/host.rs` (after a land, run each `ci:` scenario on the landed commit and keep its result; compare a failed task run by part and assert), `docs/tasks.md`. Proof: `scripts/test-task.py` stages a scenario that fails on main and in a task. The answer names the main run and starts no `check_run` fix turn. A different failure still starts one.
3. **Same-tree retry (C).** Files: `crates/wsx/src/task/host.rs` (one retry of a failed `ci:` command on an unchanged tree), `docs/tasks.md`. Proof: `scripts/test-task.py` with a scenario that fails once and then passes. The answer says "passed on retry", and the log has both results.

Measure again after a week of window B: run Q3, Q4 and Q9 with `CUT` as the start, and compare CI `ms` against Q6 times 1.55 (Q11).
