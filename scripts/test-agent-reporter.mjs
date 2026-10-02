#!/usr/bin/env node
// Exercise the shared boundary through real child execution, not mocked callbacks.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { execReporter } from "../crates/wsx-core/integrations/common/wsx-reporter.mjs";

const work = path.resolve(`.work/reporter-adapters-${process.pid}`);
fs.mkdirSync(work, { recursive: true, mode: 0o700 });
const oldSocket = process.env.WSX_SOCKET;
try {
  process.env.WSX_SOCKET = path.join(work, "wsx.sock");
  const stable = path.join(work, "wsx.reporter");
  const binary = path.join(work, "current-wsx");
  const log = path.join(work, "calls");
  fs.writeFileSync(binary, `#!/bin/sh\nprintf '%s\\n' "$*" >> '${log}'\nexit "\${REPORTER_EXIT:-0}"\n`, { mode: 0o755 });
  fs.symlinkSync(binary, stable);
  const missing = path.join(work, "removed-0.26.2/wsx");
  const invoke = (reporter, extra = {}) => new Promise(resolve => {
    execReporter(reporter, ["agent", "report", "42", "--state", "idle"],
      { timeout: 1000, env: { ...process.env, ...extra } }, error => resolve(error));
  });
  assert.equal(await invoke(missing), null);
  assert.match(fs.readFileSync(log, "utf8"), /^agent report 42 --state idle\n$/);
  const rejected = await invoke(binary, { REPORTER_EXIT: "1" });
  assert.equal(rejected.code, 1);
  assert.equal(fs.readFileSync(log, "utf8").trim().split("\n").length, 2,
    "a delivered/rejected report must not replay through the stable link");
  fs.chmodSync(work, 0o755);
  assert.equal((await invoke(missing)).code, "ENOENT");
  fs.chmodSync(work, 0o700);
  fs.chmodSync(binary, 0o777);
  assert.equal((await invoke(missing)).code, "ENOENT");
  fs.chmodSync(binary, 0o644);
  assert.equal((await invoke(binary)).code, "EACCES", "permission failures must not fall back");
  assert.equal(fs.readFileSync(log, "utf8").trim().split("\n").length, 2,
    "unsafe fallback or permission failure executed a reporter");
  fs.unlinkSync(stable);
  const unavailable = await invoke(missing);
  assert.equal(unavailable.code, "ENOENT");
  console.log("agent reporter: removed path recovers, rejected delivery does not replay, unsafe paths are refused, missing link fails boundedly");
} finally {
  if (oldSocket === undefined) delete process.env.WSX_SOCKET;
  else process.env.WSX_SOCKET = oldSocket;
  fs.rmSync(work, { recursive: true, force: true });
}
