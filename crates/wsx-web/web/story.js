// Show-me context: where the project stands (the brief, from the ledger) and where one task
// stands (its story, from the record, the result and the log), in plain words written by wsx,
// never by a model (docs/web-client.md "Project brief and task story").
import { api } from "./api.js";
import { html, Icon, useEffect, useRef, useState } from "./lib.js";
import { can } from "./store.js";
import { toast } from "./ui.js";
import { needing, taskLink } from "./tasks.js";

const plural = (count, one, many = `${one}s`) => `${count} ${count === 1 ? one : many}`;
// `refs/heads/main` reads as `main`, `refs/remotes/origin/main` as `origin/main`.
const branchName = (ref) => (ref ?? "").replace(/^refs\/(heads|remotes)\//, "");
const short = (commit) => (commit ?? "").slice(0, 8);

// An open ledger row's state in words, as the owner would say it.
export function openWords(row) {
  switch (row.state) {
    case "blocked": return "waits for you";
    case "ready": return row.human_verify > 0 && row.verification?.result !== "pass" ? "ready to verify" : "ready to land";
    case "failed": return "failed";
    case "conflict": return "conflict while landing";
    case "checking": return "running checks";
    case "in_review": return "in review";
    case "stopped": return "stopped";
    default: return "working";
  }
}

// What the deploy (or release) has, in words; null when its command is not set.
function deployWords(status, kind, default_branch) {
  if (!status?.configured) return null;
  const name = kind === "deploy" ? "Deploy" : "Release";
  if (!status.commit) return status.failed_since ? `${name}: never succeeded (${plural(status.failed_since, "failed attempt")}).` : `${name}: never run.`;
  const behind = status.behind == null ? "" : status.behind === 0 ? `, up to date with ${default_branch}` : `, ${plural(status.behind, "commit")} behind ${default_branch}`;
  const failed = status.failed_since ? ` The last ${plural(status.failed_since, "attempt")} failed.` : "";
  return `${name}: ${short(status.commit)}${behind}.${failed}`;
}

// The brief's folded line: "3 open · 1 needs you · main 2 ahead of deploy".
export function briefLine(ledger, needs) {
  const open = ledger.open.length + (ledger.open_omitted ?? 0);
  const parts = [open ? `${open} open` : "No open tasks"];
  if (needs) parts.push(`${needs} ${needs === 1 ? "needs" : "need"} you`);
  const deploy = ledger.deploys?.deploy;
  const main = branchName(ledger.deploys?.default_branch);
  if (deploy?.configured) {
    if (!deploy.commit) parts.push("never deployed");
    else if (deploy.behind > 0) parts.push(`${main} ${deploy.behind} ahead of deploy`);
    else if (deploy.behind === 0) parts.push("deploy up to date");
    if (deploy.failed_since) parts.push("last deploy failed");
  }
  return parts.join(" · ");
}

const EXPLAIN_PROJECT = "Explain where the project stands in plain words";
const explainTask = (id) => `Explain #${id} in plain words: why, what changed, risks`;

// One tap for a deeper answer: a fixed owner message to the manager, answered in the chat.
function Explain({ project, text, label, onSent }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  if (!can("task_act")) return null;
  const send = async () => {
    setBusy(true);
    setError("");
    try {
      await api(`api/tasks/${project}/say`, { method: "POST", json: { text } });
      toast("Asked the manager; the answer comes in the chat.");
      onSent?.();
    } catch (failure) {
      setError(failure.message);
    }
    setBusy(false);
  };
  return html`
    ${error && html`<span class="error" role="alert">${error}</span>`}
    <button class="button story-explain" type="button" disabled=${busy} onClick=${send} aria-label=${label}>Explain</button>`;
}

// Whether each project's brief is unfolded, for this page's life: folded by default.
const unfolded = new Set();
// ^ The brief runs git in the main checkout, so task changes refresh it at most this often.
const BRIEF_EVERY_MS = 2000;

// The project brief at the top of the chat: one line, unfolded on a tap.
export function ProjectBrief({ project, overview, onExplained }) {
  const [ledger, setLedger] = useState(null);
  const [open, setOpen] = useState(unfolded.has(project));
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
  // A changed overview (a task's state, an approval, a decision) reads the brief again.
  useEffect(() => {
    if (!last.current || timer.current) return;
    timer.current = setTimeout(() => { timer.current = null; read(); }, Math.max(0, last.current + BRIEF_EVERY_MS - Date.now()));
  }, [overview]);
  if (!ledger) return null;
  const needs = needing(project, overview).filter((item) => item.kind !== "done").length;
  const main = branchName(ledger.deploys?.default_branch) || ledger.main?.branch || "the default branch";
  const landed = ledger.recent.filter((row) => row.state === "landed").slice(0, 3);
  const deploys = ["deploy", "release"].map((kind) => deployWords(ledger.deploys?.[kind], kind, main)).filter(Boolean);
  const checkout = ledger.main?.branch
    ? `Main checkout on ${ledger.main.branch}, ${ledger.main.changed_files ? `${plural(ledger.main.changed_files, "uncommitted file")}` : "clean"}.`
    : null;
  const toggle = (event) => {
    const now = event.currentTarget.open;
    if (now) unfolded.add(project);
    else unfolded.delete(project);
    setOpen(now);
  };
  return html`
    <details class="brief" open=${open} onToggle=${toggle}>
      <summary class="brief-line"><${Icon} name="chevron" /><span class="brief-text">${briefLine(ledger, needs)}</span></summary>
      <div class="brief-body">
        ${ledger.open.length ? html`
          <ul class="brief-list" aria-label="Open tasks">
            ${ledger.open.map((row) => html`
              <li key=${row.id}><a href=${taskLink(project, row.id)}><span class="need-id">#${row.id}</span> ${row.title}</a><span class="brief-state">${openWords(row)}</span></li>`)}
            ${ledger.open_omitted ? html`<li class="muted">+${ledger.open_omitted} more</li>` : null}
          </ul>` : html`<p>Nothing is open.</p>`}
        ${landed.length ? html`
          <p class="brief-head">Landed recently</p>
          <ul class="brief-list" aria-label="Landed recently">
            ${landed.map((row) => html`<li key=${row.id}><a href=${taskLink(project, row.id)}><span class="need-id">#${row.id}</span> ${row.title}</a></li>`)}
          </ul>` : null}
        ${deploys.map((line) => html`<p key=${line}>${line}</p>`)}
        ${checkout && html`<p>${checkout}</p>`}
        <div class="brief-actions"><${Explain} project=${project} text=${EXPLAIN_PROJECT} label="Explain where the project stands" onSent=${onExplained} /></div>
      </div>
    </details>`;
}

export const STEPS = ["Asked", "Working", "Checks", "Ready", "Verified", "Landed", "Deployed"];
// Indexes into STEPS.
const [ASKED, WORKING, CHECKS, READY, VERIFIED, LANDED, DEPLOYED] = [0, 1, 2, 3, 4, 5, 6];

// Where a task is on `Asked → … → Deployed`: the last step reached, a step where it stopped
// (with the word), and steps that do not apply.
export function storySteps(detail) {
  const items = detail.result?.human_verify?.length ?? 0;
  const passed = detail.verification?.result === "pass";
  const checksFailed = (detail.checks ?? []).some((check) => check.exit !== 0);
  const skipped = new Set(items ? [] : [VERIFIED]);
  if (!detail.deploy?.configured) skipped.add(DEPLOYED);
  const at = (reached, stop = null, word = null) => ({ reached, stop, word, skipped });
  switch (detail.state) {
    case "starting": case "working": return at(WORKING);
    case "checking": return at(CHECKS);
    case "blocked": return at(ASKED, WORKING, "blocked");
    case "stopped": return at(ASKED, WORKING, "stopped");
    case "failed": return checksFailed ? at(WORKING, CHECKS, "failed") : at(ASKED, WORKING, "failed");
    case "ready": return at(items && passed ? VERIFIED : READY);
    case "conflict": return at(items ? VERIFIED : READY, LANDED, "conflict");
    case "in_review": return at(items ? VERIFIED : READY, LANDED, "in review");
    case "landed": return at(detail.deploy?.deployed ? DEPLOYED : LANDED);
    case "closed": return detail.landed_commit ? at(detail.deploy?.deployed ? DEPLOYED : LANDED) : at(ASKED, WORKING, "closed");
    default: return at(ASKED);
  }
}

// One sentence: what happens next in this state, and who acts.
export function nextWords(detail) {
  const items = detail.result?.human_verify?.length ?? 0;
  const passed = detail.verification?.result === "pass";
  const main = detail.default_branch ?? "the default branch";
  const asked = detail.approvals?.[0];
  if (asked) return `The manager asks: ${asked.label}. Approve or Deny it above.`;
  switch (detail.state) {
    case "starting": case "working": return "The worker is on it. Steer it now, or add a message for after this turn.";
    case "checking": return "The checks run on the new revision. The task is ready when they pass.";
    case "blocked": return detail.denials?.length
      ? `The worker waits for you to allow ${detail.denials[0]} or answer.`
      : "The worker waits for your answer.";
    case "stopped": return "The worker stopped. Add a message to resume it.";
    case "failed": {
      const failed = (detail.checks ?? []).filter((check) => check.exit !== 0).map((check) => check.command);
      return failed.length
        ? `${failed.length === 1 ? "A check" : "Checks"} failed (${failed.join(", ")}). Add a message so the worker fixes it.`
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

const STOP_TONE = { blocked: "attention", failed: "danger", conflict: "danger" };

// The task story at the top of the task: what was asked, the progress line, what happens next.
export function TaskStory({ project, detail, onExplained }) {
  const { reached, stop, word, skipped } = storySteps(detail);
  const current = stop ?? reached;
  return html`
    <section class="story" aria-label="Where this task stands">
      <p class="story-asked"><span class="muted">Asked:</span> ${detail.title || `task #${detail.id}`}</p>
      <ol class="story-steps" aria-label="Progress">
        ${STEPS.map((name, index) => {
          const state = index === stop ? "stopped" : skipped.has(index) && index > reached ? "skipped"
            : index <= reached ? "done" : "todo";
          return html`
            <li key=${name} data-step=${state} data-tone=${index === stop ? STOP_TONE[word] : null} aria-current=${index === current ? "step" : null}>
              <span class="story-name">${name}</span>${index === stop && html`<span class="story-word"> ${word}</span>`}
              <span class="visually-hidden">${state === "done" ? " (done)" : state === "skipped" ? " (not needed)" : state === "todo" ? " (not yet)" : ""}</span>
            </li>`;
        })}
      </ol>
      <p class="story-next"><span class="muted">What happens next:</span> ${nextWords(detail)}</p>
      <div class="story-actions"><${Explain} project=${project} text=${explainTask(detail.id)} label=${`Explain #${detail.id}`} onSent=${onExplained} /></div>
    </section>`;
}
