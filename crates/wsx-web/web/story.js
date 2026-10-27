// Show-me context: where the project stands (from the ledger) and what happens next to one
// task (from the record and the result), in plain words written by wsx, never by a model
// (docs/web-client-tasks.md "Task sheet").
import { api } from "./api.js";
import { firstSentence, html, Icon, short, useEffect, useRef, useState } from "./lib.js";
import { Markdown } from "./markdown.js";

const plural = (count, one, many = `${one}s`) => `${count} ${count === 1 ? one : many}`;
// `refs/heads/main` reads as `main`, `refs/remotes/origin/main` as `origin/main`.
const branchName = (ref) => (ref ?? "").replace(/^refs\/(heads|remotes)\//, "");

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
// (docs/web-client-tasks.md "Project page"). Read when it opens and as tasks change.
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
  // ^ The end of the newest failed attempt's output, folded under the facts (docs/tasks.md "Ledger").
  const failures = ["deploy", "release"].map((kind) => ({ kind, failed: ledger.deploys?.[kind]?.failed })).filter(({ failed }) => failed?.output);
  if (!deploys.length && !checkout) return null;
  return html`
    <div class="facts-line">
      ${deploys.map((line) => html`<p key=${line}>${line}</p>`)}
      ${failures.map(({ kind, failed }) => html`
        <details class="task-fold deploy-output" key=${kind} onToggle=${toEnd}>
          <summary class="fold-head"><${Icon} name="chevron" />How the ${kind} of ${short(failed.commit)} ended</summary>
          <div class="code-block"><pre tabindex="0">${failed.output}</pre></div>
        </details>`)}
      ${checkout && html`<p>${checkout}</p>`}
    </div>`;
}

// ^ The fold opens to the output's last lines, where a failure says why.
function toEnd(event) {
  const pre = event.currentTarget.querySelector("pre");
  if (event.currentTarget.open) pre.scrollTop = pre.scrollHeight;
}

// What the owner should know next, in one sentence, or nothing while the agents just work.
function nextWords(detail) {
  const main = detail.default_branch ?? "the default branch";
  const asked = detail.approvals?.[0];
  if (asked) return `The manager asks: ${asked.label}.`;
  switch (detail.state) {
    case "waiting": switch (detail.waiting?.on) {
      case "disk": return `${detail.waiting.reason}. It goes on by itself once there is space.`;
      case "hand": return "You work on it by hand in its session. Commit, then hand it back: its checks run and it lands as usual.";
      default: return detail.waiting?.reason === "stopped by request"
        ? "The worker stopped. Add a message to resume it." : "The worker waits for your answer.";
    }
    case "failed": {
      // ^ A check is a shell command; Evidence names them, this sentence only counts them.
      const failed = (detail.checks ?? []).filter((check) => check.exit !== 0).length;
      return failed
        ? `${failed === 1 ? "A check" : `${failed} checks`} failed after the worker's retries. Add a message so it fixes ${failed === 1 ? "it" : "them"}.`
        : "The worker could not finish. Add a message with what to do next.";
    }
    case "ready": {
      const reason = detail.land_reason ?? "";
      if (reason.startsWith("could not land: ")) return `It could not land: ${reason.slice(16)} Land it again when that is fixed.`;
      if (reason === "the manager reviews it") return `The manager reviews the work, then lands it on ${main} or sends it back.`;
      if (reason.startsWith("no checks")) return `No checks prove it, so it waits for you. Land it to put it on ${main}.`;
      return `Land it to put it on ${main}.`;
    }
    case "working": return detail.conflict ? `The worker resolves a conflict with ${main}; it lands when its checks pass.` : "";
    case "landed": case "closed": {
      if (detail.state === "closed" && !detail.landed_commit) return "Closed without landing.";
      if (!detail.deploy?.configured || detail.deploy.deployed) return `On ${main}.`;
      return `On ${main}; the last deploy does not have it yet.`;
    }
    default: return "";
  }
}

// The top of the task sheet: one sentence of what changed (More shows the whole summary and
// what the worker decided), and what the owner should know next with the task's actions beside
// it (docs/web-client-tasks.md "Task sheet").
export function TaskStory({ detail, words, children }) {
  const [more, setMore] = useState(false);
  const summary = (detail.result?.summary ?? "").trim();
  const sentence = firstSentence(summary);
  const decisions = detail.result?.decisions ?? [];
  const longer = summary !== sentence || decisions.length > 0;
  const next = nextWords(detail);
  if (!sentence && !next && !children) return null;
  return html`
    <section class="story" aria-label="Where this task stands">
      ${sentence && (more ? html`
        <div class="story-summary" id="story-more">
          <${Markdown} text=${summary} words=${words} />
          ${decisions.length ? html`<p>Decided: ${decisions.map((entry) => `${entry.choice} (${entry.reason})`).join("; ")}.</p>` : null}
        </div>` : html`<p class="story-summary">${words(sentence)}</p>`)}
      ${sentence && longer && html`<button class="link-action" type="button" aria-expanded=${more} aria-controls="story-more"
        onClick=${() => setMore(!more)}>${more ? "Less" : "More"}</button>`}
      ${(next || children) && html`<div class="story-act">${next && html`<p class="story-next">${next}</p>`}${children}</div>`}
    </section>`;
}
