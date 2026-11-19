// Show-me context: where the project stands (from the ledger) and what happens next to one
// task (from the record and the result), in plain words written by wsx, never by a model
// (docs/web-client-tasks.md "Task sheet").
import { api } from "./api.js";
import { ago, html, Icon, lastSentence, plural, short, useEffect, useRef, useState } from "./lib.js";
import { Markdown } from "./markdown.js";
import { useStore } from "./store.js";
import { agentWords, chainOf, live, rowLine, statusTone, taskLink, TaskChip, TaskTitle } from "./task-words.js";

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
  || ["owner", "plan", "paused"].includes(detail.waiting?.on);

// ^ How often the age on an open sheet is said again; only the words change, nothing is read.
const AGE_EVERY_MS = 30_000;

const cut = (text, max) => text.length > max ? `${text.slice(0, max - 1).trimEnd()}…` : text;
const fileName = (path) => path.replace(/\/+$/, "").split("/").pop();
const host = (url) => { try { return new URL(url).host; } catch { return cut(url, 40); } };
// A command without its leading `cd <dir> &&`, on one line.
const command = (text) => cut(text.replace(/^\s*cd\s+\S+\s*(?:&&|;)\s*/, "").split("\n")[0].trim(), 48);
// The tools both adapters log (claude: Read, Edit, Bash…; pi: read, bash, edit, write, grep, find,
// ls), by lower-case name: [words, how the argument reads], never a full path or a long command.
const TOOLS = {
  read: ["reading", fileName], edit: ["editing", fileName], multiedit: ["editing", fileName], notebookedit: ["editing", fileName],
  write: ["writing", fileName], bash: ["running", command], bashoutput: ["reading command output"], killshell: ["stopping a command"],
  grep: ["searching code"], glob: ["listing files"], find: ["listing files"], ls: ["listing files"],
  webfetch: ["fetching", host], websearch: ["searching the web"], task: ["asking a helper agent"], agent: ["asking a helper agent"],
  todowrite: ["updating its todo list"], skill: ["loading a skill"], toolsearch: ["looking up tools"],
};

// What a worker's newest log line says it does, in few words: a tool line ("Edit /a/b/task.js")
// as "editing task.js"; a tool of neither adapter's list by its name ("using Foo"); any other
// line, which wsx wrote, as it is, cut short.
export function doing(line) {
  const [name, ...rest] = line.trim().split(/\s+/);
  const argument = rest.join(" ");
  const tool = Object.hasOwn(TOOLS, name.toLowerCase()) ? TOOLS[name.toLowerCase()] : null;
  if (tool) return [tool[0], tool[1] && argument ? tool[1](argument) : ""].filter(Boolean).join(" ");
  // ^ A tool name is one token: capitalized or with a `_`/`:`/`.` (mcp__server__tool); wsx's own lines are sentences.
  if (/^(?:[A-Z][\w.:-]*|[a-z][a-z0-9]*[_:.][\w.:-]*)$/.test(name)) return `using ${name.split("__").pop()}`;
  return cut(line.trim(), 60);
}

// The progress line: the worker's newest tool, its own words, or the checks that run; none while
// the task waits (the status sentence under it says what for) or is ready.
function nowLine(detail, words) {
  const { now, running: checks, stuck } = detail.progress;
  if (detail.waiting) return null;
  if (detail.state === "checking" && checks?.length) return html`running ${cut(checks.join(", "), 60)}`;
  if (detail.state !== "working" && !stuck) return null;
  if (!now) return null;
  return now.said ? words(lastSentence(now.line)) : doing(now.line);
}

// The state in one line: a dot, the status label and an age, with the workflow steps and what the
// worker does now while it works (docs/web-client-tasks.md "Task sheet"). The label is the one
// word on where the task stands; the rest of the status sentence is in More.
function State({ detail, words }) {
  const [, setTick] = useState(0);
  const { stuck, last_unix_ms: last = 0, steps = [], step = 0 } = detail.progress ?? {};
  const ended = !detail.progress || ["ready", "failed"].includes(detail.state);
  // ^ The timer runs only while the age of the worker's last step shows.
  const aged = !ended && last > 0;
  useEffect(() => {
    if (!aged) return undefined;
    const timer = setInterval(() => setTick((tick) => tick + 1), AGE_EVERY_MS);
    return () => clearInterval(timer);
  }, [aged]);
  const tone = statusTone(detail);
  const since = aged ? last : detail.status.since_unix_ms;
  // ^ Checks run after the last step's result, so every step is done by then.
  const current = detail.state === "checking" ? steps.length : step;
  const working = detail.progress && ["working", "checking"].includes(detail.state);
  const now = working ? nowLine(detail, words) : null;
  return html`
    <p class="story-state">
      <span class="mark" data-tone=${tone} data-running=${live(detail) && !stuck ? "" : null} aria-hidden="true"></span>
      <span class="progress-health" data-tone=${tone}>${detail.status.label}</span>
      ${since > 0 && html` · <span class="progress-age">${ago(Date.now() - since)}</span>`}
    </p>
    ${detail.status.ask && html`<p class="story-ask">${words(detail.status.ask)}</p>`}
    ${working && steps.length > 0 && html`
      <ol class="progress-steps" aria-label="Workflow steps">
        ${steps.map((name, index) => html`
          <li key=${name} data-done=${index < current ? "" : null} aria-current=${index === current ? "step" : null}>
            ${index < current && html`<${Icon} name="pass" />`}${name}
          </li>`)}
      </ol>`}
    ${now && html`<p class="progress-now">${now}</p>`}`;
}


// The open tasks of this task's group, in the list's order: the tasks it waits for above it,
// itself, the tasks that wait for it below; each a link with its number, title and the line
// under it as the list has it (wsx-core `chains`, read from the project's list: nothing here
// sorts or counts). Nothing when the task stands alone.
function Chain({ project, detail }) {
  const tasks = useStore().tasks[project]?.tasks ?? [];
  const rows = chainOf(tasks, detail);
  if (rows.length < 2) return null;
  return html`
    <section class="task-chain" aria-label="Chain">
      <h3 class="task-chain-title">Chain</h3>
      <ol>
        ${rows.map((task) => html`
          <li key=${task.id} data-nest=${task.flow.place.depth || null}>
            <a class="chain-row" href=${taskLink(project, task.id)} aria-current=${task.id === detail.id ? "page" : null}>
              <span class="row-main"><${TaskTitle} title=${task.title} id=${task.id} />
                <span class="row-context"><span class="row-glyph"><${Icon} name=${rowLine(task).glyph} /></span>${rowLine(task).text}</span></span>
            </a>
          </li>`)}
      </ol>
    </section>`;
}

// Where the task stands among the others and where it goes (wsx-core `view::flow`, one fact a
// line): what it waits on and what it lands into while open; the chain shows who waits for it.
function Flow({ project, detail }) {
  const { flow } = detail;
  if (!flow) return null;
  const chip = (id) => html`<${TaskChip} project=${project} id=${id} />`;
  const waits = [
    flow.after && html`<span key="after">${chip(flow.after.task)} to land, ${flow.after.because}</span>`,
    flow.held && html`<span key="held">held: ${flow.held}</span>`,
  ].filter(Boolean);
  // ^ `into` is set while the task is open.
  const open = flow.into != null;
  const facts = waits.length > 0 || open || flow.parent != null;
  const list = (rows) => rows.map((row, index) => html`${index > 0 && ", "}${row}`);
  return html`
    ${facts && html`
      <dl class="facts task-flow" aria-label="Where it goes">
        ${waits.length > 0 && html`<dt>Waits on</dt><dd>${list(waits)}</dd>`}
        ${open && html`<dt>Lands into</dt><dd>${flow.parent != null ? chip(flow.parent) : flow.into}</dd>`}
        ${!open && flow.parent != null && html`<dt>Parent</dt><dd>${chip(flow.parent)}</dd>`}
      </dl>`}
    <${Chain} project=${project} detail=${detail} />`;
}

// The top of the task sheet, under the title: the state in one line, the question when the task
// asks the owner, the actions (`children`) and More with the rest: the model, where it goes, what
// comes next and the worker's summary with its decisions (docs/web-client-tasks.md "Task sheet").
export function TaskStory({ project, detail, words, children }) {
  const summary = summaryIsCurrent(detail) ? (detail.result?.summary ?? "").trim() : "";
  const decisions = detail.result?.decisions ?? [];
  const model = agentWords(detail.agent);
  const rest = detail.status.rest;
  return html`
    <section class="story" aria-label="Where this task stands">
      <${State} detail=${detail} words=${words} />
      ${children}
      <details class="task-fold story-fold">
        <summary class="fold-head"><${Icon} name="chevron" />More</summary>
        <div class="story-fold-body">
          ${model && html`<p class="progress-model" title="Model">${model}</p>`}
          <${Flow} project=${project} detail=${detail} />
          ${rest && html`<p class="story-next">${rest}</p>`}
          ${summary && html`
            <div class="story-summary">
              <${Markdown} text=${summary} words=${words} />
              ${decisions.length ? html`<p>Decided: ${decisions.map((entry) => `${entry.choice} (${entry.reason})`).join("; ")}.</p>` : null}
            </div>`}
        </div>
      </details>
    </section>`;
}
