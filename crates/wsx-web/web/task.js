// One task, in its sheet over the project chat: what happens next with its actions, then what
// the owner decides on (a brief's open questions, or the Human Verify items with Pass and Fail
// and the summary), the rest folded under Details, and the talk with the worker
// (docs/web-client.md "Task sheet").
import { api } from "./api.js";
import { html, Icon, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";
import { can, events, useStore } from "./store.js";
import { Markdown } from "./markdown.js";
import { grow, isolate, release } from "./chat.js";
import { announce, askWords, grantBody, loadTasks, taskState, TaskChip, withChips } from "./tasks.js";
import { Bubble } from "./bubbles.js";
import { nextWords } from "./story.js";

// The report as its sections; the title and state line are in the top row already.
function sections(report) {
  if (!report) return [];
  return report.split(/^## /m).slice(1).map((part) => {
    const end = part.indexOf("\n");
    return { title: part.slice(0, end).trim(), body: part.slice(end + 1).trim() };
  });
}

// An action's draft and error, kept while the page re-renders; a success clears the draft.
function useAction(detail) {
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const run = async (name, path, body, done, label = `#${detail.id}`) => {
    setBusy(name);
    setError("");
    let ok = false;
    try {
      const answer = await api(path, { method: "POST", json: body });
      announce(label, name, answer);
      done?.();
      loadTasks(detail.project);
      ok = true;
    } catch (failure) {
      setError(failure.message);
    }
    setBusy("");
    return ok;
  };
  return { busy, error, run };
}

function Verify({ detail, act }) {
  const [failing, setFailing] = useState(false);
  const note = useRef();
  useEffect(() => { if (failing) note.current.focus(); }, [failing]);
  const path = `api/tasks/${detail.project}/t/${detail.id}/verify`;
  const fail = (event) => {
    event.preventDefault();
    const text = note.current.value.trim();
    if (text) act.run("verify", path, { result: "fail", note: text }, () => setFailing(false));
  };
  return html`
    <div class="verify">
      ${failing && html`
        <form class="verify-note" onSubmit=${fail}>
          <textarea class="input" ref=${note} rows="3" aria-label="What is wrong" placeholder="What is wrong (goes to the worker)"></textarea>
          <div class="actions">
            <button class="button" type="button" onClick=${() => setFailing(false)}>Cancel</button>
            <button class="button" type="submit" disabled=${Boolean(act.busy)}>Send fail</button>
          </div>
        </form>`}
      ${!failing && html`
        <div class="actions">
          <button class="button" type="button" disabled=${Boolean(act.busy)} onClick=${() => setFailing(true)}>Fail</button>
          <button class="button" type="button" disabled=${Boolean(act.busy)}
            onClick=${() => act.run("verify", path, { result: "pass" })}>Pass r${detail.revision}</button>
        </div>`}
    </div>`;
}

// One titled part of the sheet.
const Section = ({ title, children }) => html`
  <section class="task-section" aria-label=${title}>
    <h2 class="task-heading">${title}</h2>
    <div class="task-body">${children}</div>
  </section>`;

// The rest of a task, one tap away: closed until the owner opens it.
const Details = ({ children }) => html`
  <details class="task-section task-fold">
    <summary class="task-heading fold-head"><${Icon} name="chevron" />Details</summary>
    ${children}
  </details>`;

// A term or an item that folds to one line; a tap shows what it says.
const Fold = ({ head, children }) => html`
  <details class="task-concept">
    <summary class="task-concept-term"><${Icon} name="chevron" /><span>${head}</span></summary>
    <div class="task-concept-body">${children}</div>
  </details>`;

const list = (items, item) => items?.length
  ? html`<ul class="task-brief-list">${items.map((entry, index) => html`<li key=${index}>${item(entry)}</li>`)}</ul>` : null;

// What the owner decides a brief on (docs/tasks.md "Brief"): its open questions and its
// additions, each one line that opens to its reason. An empty part is left out.
function BriefDecide({ brief, words }) {
  const prose = (text) => html`<${Markdown} text=${text} words=${words} prose />`;
  return html`
    ${brief.open_questions?.length ? html`<${Section} title="Open questions">${list(brief.open_questions, prose)}</${Section}>` : null}
    ${brief.additions?.length ? html`
      <${Section} title="Additions">
        <div class="task-concepts">${brief.additions.map((entry, index) => html`<${Fold} key=${index} head=${entry.what}>${prose(entry.why)}</${Fold}>`)}</div>
      </${Section}>` : null}`;
}

// The rest of a brief, for the Details fold. A source is a path or a link, shown as text.
function BriefRest({ brief, words }) {
  const prose = (text) => html`<${Markdown} text=${text} words=${words} prose />`;
  const parts = [
    ["Raw material", list(brief.raw_material, (entry) => html`<p class="task-source">${entry.source}</p>${prose(entry.why)}`)],
    ["Methods and tools", list(brief.methods, prose)],
    ["Independent check", list(brief.independent_check, prose)],
    ["Concepts", brief.concepts?.length ? html`
      <div class="task-concepts">${brief.concepts.map((entry, index) => html`<${Fold} key=${index} head=${entry.term}>${prose(entry.explanation)}</${Fold}>`)}</div>` : null],
    ["Rewritten prompt", brief.rewritten_prompt ? prose(brief.rewritten_prompt) : null],
  ];
  return parts.filter(([, body]) => body).map(([title, body]) => html`<${Section} key=${title} title=${title}>${body}</${Section}>`);
}

// The owner's messages to the worker and the worker's answers to the brief, as bubbles; an
// answer to the brief keeps its version ("Brief v2").
function Talk({ project, id, talk, words }) {
  const rows = talk.map((entry, index) => ({
    kind: "bubble", key: `talk-${index}`, at: entry.unix_ms ?? 0,
    author: entry.from === "owner" ? "owner" : `worker-${id}`,
    tag: entry.version ? `Brief v${entry.version}` : null,
    entry: { text: entry.text },
  }));
  rows.forEach((row, index) => {
    row.first = rows[index - 1]?.author !== row.author;
    row.last = rows[index + 1]?.author !== row.author;
  });
  return html`
    <section class="task-talk" aria-label="Messages">
      ${rows.map((row) => html`<${Bubble} key=${row.key} project=${project} row=${row} words=${words} />`)}
    </section>`;
}

// Unsent messages to a task, kept on this device by `<project>/<task>`.
const DRAFTS_KEY = "wsx-task-drafts";
const readDrafts = () => { try { return JSON.parse(localStorage.getItem(DRAFTS_KEY)) ?? {}; } catch { return {}; } };
function keepDraft(key, text) {
  const all = readDrafts();
  if (text) all[key] = text;
  else delete all[key];
  try { localStorage.setItem(DRAFTS_KEY, JSON.stringify(all)); } catch { /* kept in the field only */ }
}

// The task's message field: an add (an answer after the turn), or a steer into a running turn.
function Message({ detail, act }) {
  const field = useRef();
  const key = `${detail.project}/${detail.id}`;
  const [text, setText] = useState(() => readDrafts()[key] ?? "");
  useEffect(() => release, []);
  useLayoutEffect(() => { field.current.value = text; grow(field.current); }, []);
  const steering = detail.actions.includes("steer");
  const changed = () => {
    setText(field.current.value);
    keepDraft(key, field.current.value);
    grow(field.current);
  };
  const send = (action) => {
    const body = field.current.value.trim();
    if (!body) return;
    act.run(action, `api/tasks/${detail.project}/t/${detail.id}/${action}`, { text: body }, () => {
      field.current.value = "";
      changed();
    });
  };
  return html`
    <div class="composer task-message">
      <div class="composer-row" onPointerDown=${(event) => { if (!event.target.closest("button, textarea")) { event.preventDefault(); field.current.focus(); } }}>
        <div class="composer-input">
          <textarea class="composer-field" ref=${field} rows="1" aria-label=${`Message to #${detail.id}`}
            placeholder=${detail.phase === "brief" ? (steering ? "Reply to the worker" : "Reply to the brief")
              : detail.state === "blocked" ? "Answer the worker" : steering ? "Steer or add a message" : "Add a message"}
            onInput=${changed} onFocus=${isolate} onBlur=${release}></textarea>
        </div>
        ${steering && html`<button class="link-action task-steer" type="button" disabled=${!text.trim() || Boolean(act.busy)}
          onClick=${() => send("steer")}>Steer now</button>`}
        <button class="button button-primary composer-send" type="button" aria-label=${steering ? "Add after the turn" : "Send"}
          disabled=${!text.trim() || Boolean(act.busy)} onClick=${() => send("add")}>
          <span class="send-mark" aria-hidden="true"><${Icon} name="send" /></span><span class="send-text">${steering ? "Add" : "Send"}</span>
        </button>
      </div>
    </div>`;
}

export function TaskPage({ project, task, onClose, grip }) {
  const store = useStore();
  const [detail, setDetail] = useState(null);
  const [failure, setFailure] = useState("");
  const loading = useRef(false);
  const again = useRef(false);
  const load = async () => {
    if (loading.current) {
      again.current = true;
      return;
    }
    loading.current = true;
    try {
      do {
        again.current = false;
        setDetail(await api(`api/tasks/${project}/t/${task}`));
        setFailure("");
      } while (again.current);
    } catch (error) {
      setFailure(error.status === 404 ? "This task is not in this site's projects." : error.message);
    } finally {
      loading.current = false;
    }
  };
  useEffect(() => {
    load();
    const changed = (event) => { if (event.detail.project === project && event.detail.task === task) load(); };
    events.addEventListener("task", changed);
    return () => events.removeEventListener("task", changed);
  }, [project, task]);
  // An approval or a decision changes the overview, not this task's files.
  const overview = store.tasks[project];
  useEffect(() => { if (detail) load(); }, [overview?.decisions?.[0]?.approval, overview?.needs?.length]);
  const act = useAction(detail ?? { project, id: task });
  const words = withChips(project);

  // ^ The worker's own session, to watch it: the session lists leave task sessions out.
  const worker = store.sessions.find((session) => session.project.id === project && session.label === `task-${task}`);
  const top = html`
    <header class="bar" onPointerDown=${grip?.down} onPointerMove=${grip?.move} onPointerUp=${grip?.up} onPointerCancel=${grip?.up}>
      <button class="icon-button" type="button" aria-label="Back to the chat" onClick=${onClose}><${Icon} name="back" /></button>
      <div class="bar-title">
        <span class="bar-label">#${task}${detail?.title ? ` ${detail.title}` : ""}</span>
        ${detail && html`<span class="bar-state" data-tone=${taskState(detail.state)[1]}>${taskState(detail.state)[0]}${detail.phase === "brief"
          ? (detail.brief ? ` · brief v${detail.brief.version}` : "") : ` · r${detail.revision}`}</span>`}
      </div>
      ${worker && html`<div class="bar-tools"><a class="link-action bar-link" href=${`#/s/${worker.id}/terminal`}>Session</a></div>`}
    </header>`;
  if (!detail) return html`<div class="page">${top}<p class=${failure ? "error empty" : "empty"}>${failure || "Loading…"}</p></div>`;

  const actions = detail.actions;
  const acting = can("task_act");
  const busy = Boolean(act.busy);
  const button = (label, onClick, extra = {}) => html`
    <button class="button" type="button" disabled=${busy} onClick=${onClick} ...${extra}>${label}</button>`;
  // ^ What the block asks for, never a command; one grant per ask, the last one resumes the task.
  const asks = detail.asks ?? [];
  const grant = async (project_wide) => {
    for (const ask of asks) {
      if (!await act.run("allow", `api/tasks/${project}/t/${task}/allow`, grantBody(ask, project_wide))) return;
    }
  };
  const brief = detail.brief;
  // The sentence says what each action is for; the row under it holds the buttons alone.
  const buttons = acting ? [
    ...detail.approvals.map((request) => html`
      ${detail.approvals.length > 1 && html`<p class="task-approval">${request.label}${request.revision != null ? ` r${request.revision}` : ""}</p>`}
      ${button("Deny", () => act.run("deny", `api/tasks/${project}/approvals/${request.id}/deny`, {}, null, request.label), { "aria-label": `Deny: ${request.label}` })}
      ${button("Approve", () => act.run("approve", `api/tasks/${project}/approvals/${request.id}/approve`, {}, null, request.label), { "aria-label": request.label })}`),
    actions.includes("start") && brief && button(act.busy === "start" ? "Starting…" : "Start the build", () => act.run("start", `api/tasks/${project}/t/${task}/start`, {})),
    actions.includes("allow") && asks.length > 0 && html`
      ${button("Allow", () => grant(false), { "aria-label": `Allow #${task} to ${askWords(asks)}` })}
      ${button("Allow for project", () => grant(true), { "aria-label": `Allow every task of this project to ${askWords(asks)}` })}`,
    actions.includes("land") && button(act.busy === "land" ? "Landing…" : detail.state === "in_review" ? "Check pull request" : `Land r${detail.revision}`,
      () => act.run("land", `api/tasks/${project}/t/${task}/land`, {})),
  ].filter(Boolean) : [];
  const briefing = detail.phase === "brief";
  // The report's Human Verify and Summary come first; its other sections (Asked, Decisions,
  // Changes, Evidence, ...) and a built task's brief wait under Details.
  const report = sections(detail.report);
  const verify = report.find((section) => section.title === "Human Verify");
  const summary = report.find((section) => section.title === "Summary");
  const rest = report.filter((section) => section !== verify && section !== summary);
  const markdown = (text) => html`<${Markdown} text=${text} words=${words} prose />`;
  const looseSummary = !detail.report && detail.result?.summary
    ? [detail.result.summary, detail.result.notes].filter(Boolean).join("\n\n") : null;
  const details = [
    ...rest.map((section) => html`<${Section} key=${section.title} title=${section.title}>${markdown(section.body)}</${Section}>`),
    !briefing && brief && html`
      <h2 class="task-heading task-part" key="brief">Brief v${brief.version}</h2>
      <${BriefDecide} brief=${brief.brief} words=${words} /><${BriefRest} brief=${brief.brief} words=${words} />`,
  ].filter(Boolean);
  return html`
    <div class="page task-page">
      ${top}
      <div class="scroll">
        <section class="task-next" aria-label="What happens next">
          <p>${nextWords(detail)}</p>
          ${detail.state === "in_review" && detail.pr_url && html`<a href=${detail.pr_url} target="_blank" rel="noopener noreferrer">${detail.pr_url}</a>`}
          ${buttons.length ? html`<div class="actions">${buttons}</div>` : null}
        </section>
        ${briefing && brief && html`
          <${BriefDecide} brief=${brief.brief} words=${words} />
          <${Details}><${BriefRest} brief=${brief.brief} words=${words} /></${Details}>`}
        ${briefing && !brief && detail.state !== "failed" && html`<p class="empty">The worker is writing the brief. Nothing is built before you start it.</p>`}
        ${verify && html`
          <${Section} title="Human Verify">
            ${markdown(verify.body)}
            ${actions.includes("verify") && acting && html`<${Verify} detail=${detail} act=${act} />`}
          </${Section}>`}
        ${summary && html`<${Section} title="Summary">${markdown(summary.body)}</${Section}>`}
        ${looseSummary && html`<${Section} title="Summary">${markdown(looseSummary)}</${Section}>`}
        ${!briefing && !detail.report && !looseSummary && html`<p class="empty">No report yet: the worker has not finished a revision.</p>`}
        ${details.length ? html`<${Details}>${details}</${Details}>` : null}
        ${detail.talk.length ? html`<${Talk} project=${project} id=${task} talk=${detail.talk} words=${words} />` : null}
        ${detail.state === "conflict" && detail.overlaps.length ? html`
          <${Section} title="Shared files">
            <ul class="task-overlaps">
              ${detail.overlaps.map((overlap) => html`
                <li key=${overlap.id}>
                  <${TaskChip} project=${project} id=${overlap.id} />
                  ${" "}${overlap.count} ${overlap.count === 1 ? "path" : "paths"}${overlap.complete ? "" : " or more"}: <span class="path">${overlap.paths.join(", ")}</span>
                </li>`)}
            </ul>
          </${Section}>` : null}
      </div>
      ${act.error && html`<p class="error chat-error" role="alert">${act.error}</p>`}
      ${acting && actions.includes("add") && html`<${Message} detail=${detail} act=${act} />`}
    </div>`;
}
