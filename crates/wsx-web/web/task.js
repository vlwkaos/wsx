// One task: its actions, its report with Pass and Fail at the Human Verify items, the files it
// shares with other tasks, and its timeline (docs/web-client.md "Task page").
import { api } from "./api.js";
import { html, Icon, useEffect, useRef, useState } from "./lib.js";
import { can, events, markTaskSeen, useStore } from "./store.js";
import { Back } from "./ui.js";
import { Markdown } from "./markdown.js";
import { grow, isolate, release } from "./chat.js";
import { announce, loadTasks, taskLink, taskState, withChips } from "./tasks.js";

const short = (commit) => (commit ?? "").slice(0, 8);
function when(ms) {
  const date = new Date(ms);
  const today = new Date().toDateString() === date.toDateString();
  return today ? date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : date.toLocaleDateString([], { month: "short", day: "numeric" }) + " " + date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
const MESSAGE_WORDS = { prompt: "Asked", steer: "Steered", add: "Added", verify: "Human verification (fail)" };

// One log entry in words: [what, detail, tone].
function described(entry) {
  switch (entry.kind) {
    case "created": return [`Created on ${entry.branch}`, `from ${short(entry.base)}`];
    case "message": return [MESSAGE_WORDS[entry.mode] ?? "Message", entry.text];
    case "steps": return [`${entry.count} ${entry.count === 1 ? "step" : "steps"}`, entry.last];
    case "blocked": return ["Blocked", entry.denials.length ? `denied: ${entry.denials.join(", ")}` : "", "attention"];
    case "allowed": return ["Allowed", entry.tool];
    case "result": return [`Result: ${entry.status}`, entry.summary, entry.status === "done" ? null : "danger"];
    case "checks": {
      const failed = entry.results.filter((check) => check.exit !== 0);
      return [`Checks: ${entry.results.length - failed.length} of ${entry.results.length} passed`,
        failed.map((check) => `${check.command}${check.timed_out ? " (timed out)" : ` (exit ${check.exit})`}`).join(", "), failed.length ? "danger" : null];
    }
    case "revision": return [`Revision r${entry.revision}`, `${short(entry.commit)} · ${entry.paths_total} ${entry.paths_total === 1 ? "file" : "files"}`];
    case "stopped": return ["Stopped", entry.reason];
    case "closed": return [entry.discarded ? "Closed and discarded" : "Closed", ""];
    case "verified": return [`Verified r${entry.revision}: ${entry.result}`, entry.carried_from != null ? `carried from r${entry.carried_from}` : entry.note ?? "",
      entry.result === "pass" ? "accent" : "danger"];
    case "verify_skipped": return [`Landed r${entry.revision} without verification`, "", "attention"];
    case "conflict": return [`Conflict onto ${short(entry.onto)}`, entry.paths.join(", "), "danger"];
    case "land_stopped": return ["Landing stopped", entry.reason, "danger"];
    case "landed": return ["Landed", short(entry.commit), "accent"];
    case "in_review": return [`Pull request r${entry.revision}`, entry.url];
    case "pr_closed": return ["Pull request closed", entry.url, "attention"];
    default: return [entry.kind, ""];
  }
}

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
    try {
      const answer = await api(path, { method: "POST", json: body });
      announce(label, name, answer);
      done?.();
      loadTasks(detail.project);
    } catch (failure) {
      setError(failure.message);
    }
    setBusy("");
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

// The task's message field: an add (an answer after the turn), or a steer into a running turn.
function Message({ detail, act }) {
  const field = useRef();
  const [text, setText] = useState("");
  useEffect(() => release, []);
  const steering = detail.actions.includes("steer");
  const send = (action) => {
    const body = field.current.value.trim();
    if (!body) return;
    act.run(action, `api/tasks/${detail.project}/t/${detail.id}/${action}`, { text: body }, () => {
      field.current.value = "";
      setText("");
      grow(field.current);
    });
  };
  return html`
    <div class="composer task-message">
      <div class="composer-row" onPointerDown=${(event) => { if (!event.target.closest("button, textarea")) { event.preventDefault(); field.current.focus(); } }}>
        <div class="composer-input">
          <textarea class="composer-field" ref=${field} rows="1" aria-label=${`Message to #${detail.id}`}
            placeholder=${detail.state === "blocked" ? "Answer the worker" : steering ? "Steer or add a message" : "Add a message"}
            onInput=${() => { setText(field.current.value); grow(field.current); }} onFocus=${isolate} onBlur=${release}></textarea>
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

export function TaskPage({ project, task }) {
  const store = useStore();
  const [detail, setDetail] = useState(null);
  const [failure, setFailure] = useState("");
  const [tool, setTool] = useState(null);
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
  // "Done, not seen" clears on this device once the page of a landed task was open.
  useEffect(() => { if (detail?.state === "landed") markTaskSeen(project, task, detail.updated_unix_ms); }, [detail?.state, detail?.updated_unix_ms]);
  const act = useAction(detail ?? { project, id: task });
  const words = withChips(project);

  const top = html`
    <header class="bar">
      <${Back} fallback=${`#/p/${project}`} />
      <div class="bar-title">
        <span class="bar-label">#${task}${detail?.title ? ` ${detail.title}` : ""}</span>
        ${detail && html`<span class="bar-state" data-tone=${taskState(detail.state)[1]}>${taskState(detail.state)[0]} · r${detail.revision}</span>`}
      </div>
    </header>`;
  if (!detail) return html`<div class="page">${top}<p class=${failure ? "error empty" : "empty"}>${failure || "Loading…"}</p></div>`;

  const actions = detail.actions;
  const approvals = detail.approvals.map((request) => html`
    <div class="task-row" key=${request.id}>
      <p>The manager asks: ${request.label}${request.revision != null ? ` r${request.revision}` : ""}.</p>
      ${can("task_act") && html`
        <div class="actions">
          <button class="button" type="button" disabled=${Boolean(act.busy)}
            onClick=${() => act.run("deny", `api/tasks/${project}/approvals/${request.id}/deny`, {}, null, request.label)}>Deny</button>
          <button class="button" type="button" disabled=${Boolean(act.busy)}
            onClick=${() => act.run("approve", `api/tasks/${project}/approvals/${request.id}/approve`, {}, null, request.label)}>Approve</button>
        </div>`}
    </div>`);
  const allow = actions.includes("allow") && html`
    <div class="task-row">
      <p>${detail.denials?.length ? html`Denied: ${detail.denials.map((denial) => html`<code key=${denial}>${denial}</code> `)}` : "Blocked; the worker asks in its summary."}</p>
      ${tool === null
        ? html`<div class="actions"><button class="button" type="button" onClick=${() => setTool(detail.denials?.[0] ?? "")}>Allow a tool</button></div>`
        : html`
          <form class="need-form task-allow" onSubmit=${(event) => { event.preventDefault(); const value = event.target.tool.value.trim();
            if (value) act.run("allow", `api/tasks/${project}/t/${task}/allow`, { tool: value }, () => setTool(null)); }}>
            <input class="input" name="tool" type="text" autocomplete="off" value=${tool} aria-label="Tool to allow" placeholder="Tool, such as Bash(make:*)" />
            <button class="button" type="submit" disabled=${Boolean(act.busy)}>Allow</button>
          </form>`}
    </div>`;
  const land = actions.includes("land") && html`
    <div class="task-row">
      <p>${detail.state === "in_review" ? html`In review: <a href=${detail.pr_url} target="_blank" rel="noopener noreferrer">${detail.pr_url}</a>` : `Ready to land r${detail.revision} on ${detail.default_branch}.`}</p>
      <div class="actions"><button class="button" type="button" disabled=${Boolean(act.busy)}
        onClick=${() => act.run("land", `api/tasks/${project}/t/${task}/land`, {})}>${act.busy === "land" ? "Landing…" : detail.state === "in_review" ? "Check pull request" : `Land r${detail.revision}`}</button></div>
    </div>`;
  // The verdict is the main action of a task to verify, but its items are at the report's end.
  const items = detail.result?.human_verify?.length ?? 0;
  const review = actions.includes("verify") && html`
    <div class="task-row">
      <p>${items} Human Verify ${items === 1 ? "item waits" : "items wait"} for your verdict on r${detail.revision}.</p>
      <div class="actions"><button class="button" type="button"
        onClick=${() => document.getElementById("human-verify")?.scrollIntoView({ block: "start" })}>Review</button></div>
    </div>`;
  const entries = detail.timeline.entries;
  return html`
    <div class="page task-page">
      ${top}
      <div class="scroll">
        ${(approvals.length || allow || land || review) && html`
          <section class="task-actions" aria-label="Actions">${approvals}${allow}${review}${land}</section>`}
        ${sections(detail.report).map((section) => html`
          <section class="task-section" key=${section.title} aria-label=${section.title}
            id=${section.title === "Human Verify" ? "human-verify" : null}>
            <h2 class="label">${section.title}</h2>
            <div class="task-body"><${Markdown} text=${section.body} words=${words} /></div>
            ${section.title === "Human Verify" && actions.includes("verify") && can("task_act") && html`<${Verify} detail=${detail} act=${act} />`}
          </section>`)}
        ${!detail.report && detail.result?.summary ? html`
          <section class="task-section" aria-label="The worker says">
            <h2 class="label">The worker says</h2>
            <div class="task-body"><${Markdown} text=${[detail.result.summary, detail.result.notes].filter(Boolean).join("\n\n")} words=${words} /></div>
          </section>` : null}
        ${!detail.report && !detail.result?.summary && html`<p class="empty">No report yet: the worker has not finished a revision.</p>`}
        ${detail.overlaps.length ? html`
          <section class="task-section" aria-label="Shared files">
            <h2 class="label">Shared files</h2>
            <ul class="task-overlaps">
              ${detail.overlaps.map((overlap) => html`
                <li key=${overlap.id}>
                  <a class="task-chip" href=${taskLink(project, overlap.id)}><span class="task-chip-id">#${overlap.id}</span></a>
                  ${" "}${overlap.count} ${overlap.count === 1 ? "path" : "paths"}${overlap.complete ? "" : " or more"}: <span class="path">${overlap.paths.join(", ")}</span>
                </li>`)}
            </ul>
          </section>` : null}
        <details class="task-section timeline">
          <summary class="label timeline-head"><${Icon} name="chevron" />Timeline · ${entries.length} ${entries.length === 1 ? "entry" : "entries"}</summary>
          ${detail.timeline.skipped ? html`<p class="note">${detail.timeline.skipped} earlier entries are not shown.</p>` : null}
          <ol class="timeline-list">
            ${entries.map((entry, index) => {
              const [what, more, tone] = described(entry);
              return html`
                <li key=${index}>
                  <span class="time">${when(entry.unix_ms)}</span>
                  <span class="timeline-what" data-tone=${tone}>${what}</span>
                  ${more && html`<span class="timeline-more">${more}</span>`}
                </li>`;
            })}
          </ol>
        </details>
      </div>
      ${act.error && html`<p class="error chat-error" role="alert">${act.error}</p>`}
      ${can("task_act") && actions.includes("add") && html`<${Message} detail=${detail} act=${act} />`}
    </div>`;
}
