// Show-me context: where the project stands (from the ledger) and what happens next to one
// task (from the record and the result), in plain words written by wsx, never by a model
// (docs/web-client-tasks.md "Task sheet").
import { api } from "./api.js";
import { ago, firstSentence, html, Icon, lastSentence, short, useEffect, useRef, useState } from "./lib.js";
import { Markdown } from "./markdown.js";
import { agentWords, running, statusTone } from "./tasks.js";

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

// The deploy, the release and the main checkout in words, at the top of the project overview
// (docs/web-client-tasks.md "Project overview"). Read when it opens and as tasks change.
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

// A summary is the current word on a task only while no new work runs after it: the worker's
// answer or result, not an older revision's.
export const summaryIsCurrent = (detail) => ["ready", "failed", "landed", "closed"].includes(detail.state)
  || ["owner", "plan"].includes(detail.waiting?.on);

// ^ How often the age on an open sheet is said again; only the words change, nothing is read.
const AGE_EVERY_MS = 30_000;

// An open task at a glance: how it is doing, how long since the worker last did something, its
// workflow steps and the worker's last sentence (docs/web-client-tasks.md "Task sheet").
function Progress({ detail, words }) {
  const [, setTick] = useState(0);
  const health = [detail.status.label, statusTone(detail)];
  const { steps, step, stuck, last_unix_ms: last, said } = detail.progress;
  const working = ["working", "checking"].includes(detail.state);
  const ended = ["ready", "failed"].includes(detail.state);
  // ^ The timer runs only while the age shows.
  const aged = !ended && last > 0;
  useEffect(() => {
    if (!aged) return undefined;
    const timer = setInterval(() => setTick((tick) => tick + 1), AGE_EVERY_MS);
    return () => clearInterval(timer);
  }, [aged]);
  // ^ Checks run after the last step's result, so every step is done by then.
  const current = detail.state === "checking" || detail.state === "ready" ? steps.length : step;
  const sentence = (working || stuck) && said ? lastSentence(said) : "";
  return html`
    <div class="progress">
      <p class="progress-head">
        <span class="mark" data-tone=${health[1]} data-running=${running(detail) && working && !stuck ? "" : null} aria-hidden="true"></span>
        <span class="progress-health" data-tone=${health[1]}>${health[0]}</span>
        ${agentWords(detail.agent) && html`<span class="progress-model" title="Model">${agentWords(detail.agent)}</span>`}
        ${aged && html`<span class="progress-age">${ago(Date.now() - last)}</span>`}
      </p>
      ${steps.length > 0 && html`
        <ol class="progress-steps" aria-label="Workflow steps">
          ${steps.map((name, index) => html`
            <li key=${name} data-done=${index < current ? "" : null} aria-current=${index === current ? "step" : null}>
              ${index < current && html`<${Icon} name="pass" />`}${name}
            </li>`)}
        </ol>`}
      ${sentence && html`<p class="progress-said">${words(sentence)}</p>`}
    </div>`;
}

// The top of the task sheet: how an open task is doing, what changed (the whole summary and what
// the worker decided, open from the start; Less folds it to one sentence), and what the owner
// should know next with the task's actions beside it (docs/web-client-tasks.md "Task sheet").
export function TaskStory({ detail, words, children }) {
  const [more, setMore] = useState(true);
  const summary = summaryIsCurrent(detail) ? (detail.result?.summary ?? "").trim() : "";
  const sentence = firstSentence(summary);
  const decisions = detail.result?.decisions ?? [];
  const longer = summary !== sentence || decisions.length > 0;
  // ^ What it waits for, why, how long and what comes next (docs/tasks.md "Status").
  const next = detail.status?.sentence ?? "";
  if (!detail.progress && !sentence && !next && !children) return null;
  return html`
    <section class="story" aria-label="Where this task stands">
      ${detail.progress && html`<${Progress} detail=${detail} words=${words} />`}
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
