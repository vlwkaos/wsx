// One task, in its sheet over the project chat: what changed and the one action when it needs
// the owner, then a brief's open questions and additions or the report folded (Changes,
// Evidence, To check, Left open), the talk with the worker and its history
// (docs/web-client.md "Task sheet").
import { api } from "./api.js";
import { clock, html, Icon, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";
import { can, events, useStore } from "./store.js";
import { Markdown } from "./markdown.js";
import { grow, isolate, release } from "./chat.js";
import { announce, askWords, grantBody, loadTasks, taskStage, TaskChip, withChips } from "./tasks.js";
import { Bubble } from "./bubbles.js";
import { TaskStory } from "./story.js";
import { CheckRow, TryButton } from "./checks.js";

const short = (commit) => (commit ?? "").slice(0, 8);
const MESSAGE_WORDS = { prompt: "Asked", steer: "Steered", add: "Added" };

// One log entry in words: [what, detail, tone].
function described(entry) {
  switch (entry.kind) {
    case "created": return [`Created on ${entry.branch}`, `from ${short(entry.base)}`];
    case "message": return [MESSAGE_WORDS[entry.mode] ?? "Message", entry.text];
    case "steps": return [`${entry.count} ${entry.count === 1 ? "step" : "steps"}`, entry.last];
    case "blocked": return ["Blocked", entry.asks?.length ? `needs to ${askWords(entry.asks)}`
      : entry.denials.length ? `denied: ${entry.denials.join(", ")}` : "", "attention"];
    case "allowed": return ["Allowed", entry.tool];
    case "granted": return [`Allowed by the ${entry.by}${entry.project ? " for every task" : ""}`, askWords([entry.access]) || `use ${entry.access.rule}`];
    case "revoked": return [`Taken back by the ${entry.by}`, askWords([entry.access]) || `use ${entry.access.rule}`];
    case "result": return [`Result: ${entry.status}`, entry.summary, entry.status === "done" ? null : "danger"];
    case "checks": {
      const failed = entry.results.filter((check) => check.exit !== 0);
      return [`Checks: ${entry.results.length - failed.length} of ${entry.results.length} passed`,
        failed.map((check) => `${check.command}${check.timed_out ? " (timed out)" : ` (exit ${check.exit})`}`).join(", "), failed.length ? "danger" : null];
    }
    case "check_retry": return [`Checks failed on r${entry.revision}: sent back to the worker`,
      `${entry.commands.join(", ")} (automatic retry ${entry.attempt} of 2)`, "attention"];
    case "revision": return [`Revision r${entry.revision}`, `${short(entry.commit)} · ${entry.paths_total} ${entry.paths_total === 1 ? "file" : "files"}`];
    case "stopped": return ["Stopped", entry.reason];
    case "closed": return [entry.discarded ? "Closed and discarded" : "Closed", ""];
    // ^ Per-task verdicts from before M4f show nothing (docs/tasks.md "To check").
    case "verified": case "verify_skipped": return null;
    case "conflict": return [`Conflict onto ${short(entry.onto)}`, entry.paths.join(", "), "danger"];
    case "land_stopped": return ["Landing stopped", entry.reason, "danger"];
    case "landed": return ["Landed", short(entry.commit), "accent"];
    case "in_review": return [`Pull request r${entry.revision}`, entry.url];
    case "pr_closed": return ["Pull request closed", entry.url, "attention"];
    case "brief": return [`Brief v${entry.version}`, entry.denials?.length ? `denied: ${entry.denials.join(", ")}` : entry.answer ?? ""];
    case "brief_skipped": return ["Built without a brief", `${entry.by === "flag" ? "--no-brief" : "tasks.brief: false"} by the ${entry.actor}`];
    case "started": return [`Build started from brief v${entry.version}`, entry.prompt, "accent"];
    default: return [entry.kind, ""];
  }
}

// The report by section name; the title and the state are in the top row already.
function sections(report) {
  if (!report) return {};
  return Object.fromEntries(report.split(/^## /m).slice(1).map((part) => {
    const end = part.indexOf("\n");
    return [part.slice(0, end).trim(), part.slice(end + 1).trim()];
  }));
}

// The report's folds, in the order of report.md after its summary; sections of older reports
// (Asked, Decisions, Human Verify) are not shown.
const FOLDS = ["Changes", "Evidence", "To check", "Left open"];
const items = (body) => body.split("\n").filter((line) => line.startsWith("- ")).length;
// A fold's text: Evidence without its first line (the note says it); To check items before
// the landing as plain items, not checkboxes nobody can tick.
const foldBody = (title, body) => title === "Evidence" ? body.split("\n").slice(1).join("\n").trim()
  : title === "To check" ? body.replace(/^- \[ \] /gm, "- ") : body;
// What a fold says while closed: "4 checks passed", "2 open", "nothing".
function foldNote(title, body, detail) {
  if (title === "Evidence") return body.split("\n")[0].replace(/\.$/, "").replace(/ \(`tasks.checks`\)$/, "");
  if (title === "To check") {
    const open = detail.to_check.filter((item) => item.state === "open").length;
    if (detail.to_check.length) return open ? `${open} open` : "all checked";
    return items(body) ? `${items(body)} after it lands` : "nothing";
  }
  if (title === "Left open") return items(body) ? String(items(body)) : "nothing";
  return "";
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
        ${detail && html`<span class="bar-state" data-tone=${taskStage(detail)[1]}>${taskStage(detail)[0]}${detail.phase === "brief" && detail.brief
          ? ` · brief v${detail.brief.version}` : ""}</span>`}
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
  // ^ The story above says what each action is for; the row holds the buttons alone.
  const buttons = acting ? [
    ...detail.approvals.map((request) => html`
      ${detail.approvals.length > 1 && html`<p class="task-approval">${request.label}</p>`}
      ${button("Deny", () => act.run("deny", `api/tasks/${project}/approvals/${request.id}/deny`, {}, null, request.label), { "aria-label": `Deny: ${request.label}` })}
      ${button("Approve", () => act.run("approve", `api/tasks/${project}/approvals/${request.id}/approve`, {}, null, request.label), { "aria-label": request.label })}`),
    actions.includes("start") && brief && button(act.busy === "start" ? "Starting…" : "Start the build", () => act.run("start", `api/tasks/${project}/t/${task}/start`, {})),
    actions.includes("allow") && asks.length > 0 && html`
      ${button("Allow", () => grant(false), { "aria-label": `Allow #${task} to ${askWords(asks)}` })}
      ${button("Allow for project", () => grant(true), { "aria-label": `Allow every task of this project to ${askWords(asks)}` })}`,
    actions.includes("land") && button(act.busy === "land" ? "Landing…" : detail.state === "in_review" ? "Check pull request" : "Land",
      () => act.run("land", `api/tasks/${project}/t/${task}/land`, {})),
    store.tasks[project]?.preview && detail.phase !== "brief" && detail.state !== "closed" && html`<${TryButton} project=${project} task=${task} />`,
  ].filter(Boolean) : [];
  const briefing = detail.phase === "brief";
  const report = sections(detail.report);
  const entries = detail.timeline.entries;
  return html`
    <div class="page task-page">
      ${top}
      <div class="scroll">
        <${TaskStory} detail=${detail} words=${words} />
        ${(buttons.length || detail.state === "in_review") ? html`
          <section class="task-next" aria-label="Actions">
            ${detail.state === "in_review" && detail.pr_url && html`<p>In review: <a href=${detail.pr_url} target="_blank" rel="noopener noreferrer">${detail.pr_url}</a></p>`}
            ${buttons.length ? html`<div class="actions">${buttons}</div>` : null}
          </section>` : null}
        ${briefing && brief && html`
          <${BriefDecide} brief=${brief.brief} words=${words} />
          <${Details}><${BriefRest} brief=${brief.brief} words=${words} /></${Details}>`}
        ${briefing && !brief && detail.state !== "failed" && html`<p class="empty">The worker is writing the brief. The build starts by itself unless it has questions for you.</p>`}

        ${FOLDS.filter((title) => report[title] != null).map((title) => {
          const note = foldNote(title, report[title], detail);
          // ^ Landed items are live: Pass and Fail act on the project's list.
          const live = title === "To check" && detail.to_check.length;
          return html`
            <details class="task-section task-fold" key=${title}>
              <summary class="task-heading fold-head"><${Icon} name="chevron" />${title}${note && html`<span class="fold-note"> · ${note}</span>`}</summary>
              ${live ? html`
                <ul class="check-list">${detail.to_check.map((item) => html`
                  <${CheckRow} key=${item.id} project=${project} item=${item} revision=${detail.revision} onChanged=${load} />`)}
                </ul>`
                : html`<div class="task-body"><${Markdown} text=${foldBody(title, report[title])} words=${words} prose /></div>`}
            </details>`;
        })}
        ${!briefing && !detail.report && !detail.result?.summary && html`<p class="empty">No report yet: the worker has not finished a revision.</p>`}
        ${!briefing && brief && html`
          <details class="task-section task-fold">
            <summary class="task-heading fold-head"><${Icon} name="chevron" />Brief v${brief.version}</summary>
            <${BriefDecide} brief=${brief.brief} words=${words} /><${BriefRest} brief=${brief.brief} words=${words} />
          </details>`}
        ${detail.talk.length ? html`<${Talk} project=${project} id=${task} talk=${detail.talk} words=${words} />` : null}
        ${detail.state === "conflict" && detail.overlaps.length ? html`
          <section class="task-section" aria-label="Shared files">
            <h2 class="task-heading">Shared files</h2>
            <ul class="task-overlaps">
              ${detail.overlaps.map((overlap) => html`
                <li key=${overlap.id}>
                  <${TaskChip} project=${project} id=${overlap.id} />
                  ${" "}${overlap.count} ${overlap.count === 1 ? "path" : "paths"}${overlap.complete ? "" : " or more"}: <span class="path">${overlap.paths.join(", ")}</span>
                </li>`)}
            </ul>
          </section>` : null}
        <details class="task-section timeline">
          <summary class="task-heading fold-head"><${Icon} name="chevron" />History</summary>
          ${detail.timeline.skipped ? html`<p class="note">${detail.timeline.skipped} earlier entries are not shown.</p>` : null}
          <ol class="timeline-list">
            ${entries.map((entry, index) => {
              const said = described(entry);
              if (!said) return null;
              const [what, more, tone] = said;
              return html`
                <li key=${index}>
                  <span class="time">${clock(entry.unix_ms)}</span>
                  <span class="timeline-what" data-tone=${tone}>${what}</span>
                  ${more && html`<span class="timeline-more">${more}</span>`}
                </li>`;
            })}
          </ol>
        </details>
      </div>
      ${act.error && html`<p class="error chat-error" role="alert">${act.error}</p>`}
      ${acting && actions.includes("add") && html`<${Message} detail=${detail} act=${act} />`}
    </div>`;
}
