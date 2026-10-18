// Show-me context: where the project stands (from the ledger) and what happens next to one
// task (from the record and the result), in plain words written by wsx, never by a model
// (docs/web-client.md "Project brief and task story").
import { api } from "./api.js";
import { html, useEffect, useRef, useState } from "./lib.js";
import { askWords } from "./tasks.js";

const plural = (count, one, many = `${one}s`) => `${count} ${count === 1 ? one : many}`;
// `refs/heads/main` reads as `main`, `refs/remotes/origin/main` as `origin/main`.
const branchName = (ref) => (ref ?? "").replace(/^refs\/(heads|remotes)\//, "");
const short = (commit) => (commit ?? "").slice(0, 8);

// What the deploy (or release) has, in words; null when its command is not set.
function deployWords(status, kind, default_branch) {
  if (!status?.configured) return null;
  const name = kind === "deploy" ? "Deploy" : "Release";
  if (!status.commit) return status.failed_since ? `${name}: never succeeded (${plural(status.failed_since, "failed attempt")}).` : `${name}: never run.`;
  const behind = status.behind == null ? "" : status.behind === 0 ? `, up to date with ${default_branch}` : `, ${plural(status.behind, "commit")} behind ${default_branch}`;
  const failed = status.failed_since ? ` The last ${plural(status.failed_since, "attempt")} failed.` : "";
  return `${name}: ${short(status.commit)}${behind}.${failed}`;
}

// ^ The facts read git in the main checkout, so task changes refresh them at most this often.
const FACTS_EVERY_MS = 2000;

// The deploy, the release and the main checkout in words, at the top of the project page
// (docs/web-client.md "Project page"). Read when it opens and as tasks change.
export function ProjectFacts({ project, overview }) {
  const [ledger, setLedger] = useState(null);
  const timer = useRef(null);
  const last = useRef(0);
  const live = useRef(true);
  const read = () => {
    last.current = Date.now();
    api(`api/tasks/${project}/brief`).then((value) => { if (live.current) setLedger(value); }, () => {});
  };
  useEffect(() => {
    live.current = true;
    read();
    return () => {
      live.current = false;
      clearTimeout(timer.current);
      timer.current = null;
    };
  }, [project]);
  useEffect(() => {
    if (!last.current || timer.current) return;
    timer.current = setTimeout(() => { timer.current = null; read(); }, Math.max(0, last.current + FACTS_EVERY_MS - Date.now()));
  }, [overview]);
  if (!ledger) return null;
  const main = branchName(ledger.deploys?.default_branch) || ledger.main?.branch || "the default branch";
  const deploys = ["deploy", "release"].map((kind) => deployWords(ledger.deploys?.[kind], kind, main)).filter(Boolean);
  const checkout = ledger.main?.branch
    ? `Main checkout on ${ledger.main.branch}, ${ledger.main.changed_files ? `${plural(ledger.main.changed_files, "uncommitted file")}` : "clean"}.`
    : null;
  if (!deploys.length && !checkout) return null;
  return html`
    <div class="facts-line">
      ${deploys.map((line) => html`<p key=${line}>${line}</p>`)}
      ${checkout && html`<p>${checkout}</p>`}
    </div>`;
}

// One sentence: what happens next in this state, and who acts.
export function nextWords(detail) {
  const items = detail.result?.human_verify?.length ?? 0;
  const passed = detail.verification?.result === "pass";
  const main = detail.default_branch ?? "the default branch";
  const asked = detail.approvals?.[0];
  if (asked) return `The manager asks: ${asked.label}. Approve or Deny it below.`;
  if (detail.phase === "brief") {
    switch (detail.state) {
      case "briefed": return "The brief is ready. Read it, reply to talk it through, or Start the build.";
      case "failed": return "The worker returned no brief. Reply to ask again.";
      case "stopped": return "The worker stopped. Reply to resume the brief.";
      case "closed": return "Closed before the build. Nothing more happens.";
      default: return "The worker reads the project and writes the brief. Nothing is built before you Start.";
    }
  }
  switch (detail.state) {
    case "starting": case "working": return "The worker is on it. Steer it now, or add a message for after this turn.";
    case "checking": return "The checks run on the new revision. The task is ready when they pass.";
    case "blocked": return detail.asks?.length
      ? `The worker waits for you to let it ${askWords(detail.asks)}, or to answer.`
      : "The worker waits for your answer.";
    case "stopped": return "The worker stopped. Add a message to resume it.";
    case "failed": {
      // ^ A check is a shell command; the report names them, this sentence only counts them.
      const failed = (detail.checks ?? []).filter((check) => check.exit !== 0).length;
      return failed
        ? `${failed === 1 ? "A check" : `${failed} checks`} failed. Add a message so the worker fixes it.`
        : "The worker could not finish. Add a message with what to do next.";
    }
    case "ready": return items && !passed
      ? `Check the ${plural(items, "item")} below, then Pass to allow landing.`
      : `Land r${detail.revision} to put it on ${main}.`;
    case "conflict": return `Landing met a conflict with ${main}. The worker resolves it; after the checks it is ready to land again.`;
    case "in_review": return "The pull request waits for review. Check pull request to see whether it merged.";
    case "landed": case "closed": {
      if (detail.state === "closed" && !detail.landed_commit) return "Closed without landing. Nothing more happens.";
      if (!detail.deploy?.configured) return `Landed on ${main}. Nothing more to do.`;
      if (detail.deploy.deployed) return "Landed and deployed. Nothing more to do.";
      const behind = detail.deploy.behind ? ` (the deploy is ${plural(detail.deploy.behind, "commit")} behind)` : "";
      return `Landed on ${main}; the last deploy does not have it yet${behind}.`;
    }
    default: return "";
  }
}
