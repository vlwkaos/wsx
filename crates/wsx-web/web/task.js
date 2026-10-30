// One task, in its sheet over the project chat: how it is doing and the one action when it needs
// the owner, the To check items to act on, then one folded Details with the report, the messages
// to the worker, its usage and its history (docs/web-client-tasks.md "Task sheet").
import { api } from "./api.js";
import { clock, html, Icon, keepIn, Loading, readMap, short, useCoalescedLoad, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";
import { can, useEventLoad, useStore } from "./store.js";
import { Markdown } from "./markdown.js";
import { grow, isolate, release } from "./chat.js";
import { announce, loadTasks, taskStage, TaskChip, TaskTitle, withChips } from "./tasks.js";
import { Bubble } from "./bubbles.js";
import { SheetBar } from "./ui.js";
import { summaryIsCurrent, TaskStory } from "./story.js";
import { CheckRow, TryButton } from "./checks.js";
import { usageLine } from "./usage.js";

const TRIAL_WORDS = {
  preparing: "Going live: rebasing onto the default branch and checking",
  deploying: "Going live: deploying",
  live: "On the live site now: try it, then Pass, or send the worker what is wrong",
};

const MESSAGE_WORDS = { prompt: "Asked", steer: "Steered", add: "Added" };

// One log entry in words: [what, detail, tone].
function described(entry) {
  switch (entry.kind) {
    case "created": return [`Created on ${entry.branch}`, `from ${short(entry.base)}`];
    case "message": return [MESSAGE_WORDS[entry.mode] ?? "Message", entry.text];
    case "steps": return [`${entry.count} ${entry.count === 1 ? "step" : "steps"}`, entry.last];
    case "blocked": return ["Blocked", entry.denials.length ? `denied: ${entry.denials.join(", ")}` : "", "attention"];
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
    case "conflict": return [`Conflict onto ${short(entry.onto)}`, entry.paths.join(", "), "danger"];
    case "land_stopped": return ["Landing stopped", entry.reason, "danger"];
    case "landed": return ["Landed", short(entry.commit), "accent"];
    case "hand": return ["Worked by hand", entry.dropped ? `${entry.dropped} queued ${entry.dropped === 1 ? "message" : "messages"} dropped` : ""];
    case "handback": return ["Handed back", `${entry.commits} ${entry.commits === 1 ? "commit" : "commits"}`];
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

// The messages added or steered to the worker after its prompt, as bubbles.
function Talk({ project, talk, words }) {
  const rows = talk.map((entry, index) => ({
    kind: "bubble", key: `talk-${index}`, at: entry.unix_ms ?? 0, author: "owner",
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

// The task's message field: an add (an answer after the turn), or a steer into a running turn.
function Message({ detail, act }) {
  const field = useRef();
  const key = `${detail.project}/${detail.id}`;
  const [text, setText] = useState(() => readMap(DRAFTS_KEY)[key] ?? "");
  useEffect(() => release, []);
  useLayoutEffect(() => { field.current.value = text; grow(field.current); }, []);
  const steering = detail.actions.includes("steer");
  const changed = () => {
    setText(field.current.value);
    keepIn(DRAFTS_KEY, key, field.current.value);
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
            placeholder=${detail.waiting?.on === "owner" ? "Answer the worker" : steering ? "Steer or add a message" : "Add a message"}
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

// ^ The last read of each task this page made, so a sheet opens on its content at once and
// reads again behind it; the open decision list reads its tasks ahead (docs/web-client-tasks.md
// "Task sheet"). Kept for the page's life, at most DETAILS_KEPT, oldest out first.
const details = new Map();
const DETAILS_KEPT = 40;
const keepDetail = (key, value) => {
  details.delete(key);
  details.set(key, value);
  if (details.size > DETAILS_KEPT) details.delete(details.keys().next().value);
};
export function readTaskAhead(project, task) {
  const key = `${project}/${task}`;
  if (details.has(key)) return;
  api(`api/tasks/${project}/t/${task}`).then((value) => { if (!details.has(key)) keepDetail(key, value); }, () => {});
}

export function TaskPage({ project, task, onClose, grip }) {
  const store = useStore();
  const [detail, setDetail] = useState(() => details.get(`${project}/${task}`) ?? null);
  const [failure, setFailure] = useState("");
  const load = useCoalescedLoad(async (current) => {
    const next = await api(`api/tasks/${project}/t/${task}`);
    if (!current()) return;
    keepDetail(`${project}/${task}`, next);
    setDetail(next);
    setFailure("");
  }, (error) => setFailure(error.status === 404 ? "This task is not in this site's projects." : error.message), [project, task]);
  // ^ A silent task raises no `task` event: the manager's event on a stall reads it again.
  useEventLoad(["task", "manager"], load, (detail) => detail.project == null || (detail.project === project && (detail.task == null || detail.task === task)), [project, task]);
  // An approval or a decision changes the overview, not this task's files.
  const overview = store.tasks[project];
  useEffect(() => { if (detail) load(); }, [overview?.decisions?.[0]?.approval, overview?.needs?.length]);
  const act = useAction(detail ?? { project, id: task });
  const words = withChips(project);

  const top = html`
    <${SheetBar} grip=${grip} onClose=${onClose}>
      ${detail?.title ? html`<${TaskTitle} title=${detail.title} id=${task} />` : html`<span class="bar-label">#${task}</span>`}
      ${detail && !detail.progress && html`<span class="bar-state" data-tone=${taskStage(detail)[1]}>${taskStage(detail)[0]}</span>`}
    </${SheetBar}>`;
  if (!detail) return html`<div class="page">${top}${failure ? html`<p class="error empty">${failure}</p>` : html`<${Loading} />`}</div>`;

  const actions = detail.actions;
  const trial = detail.trial;
  const acting = can("task_act");
  const busy = Boolean(act.busy);
  // ^ With an icon, a phone shows the icon alone and its words stay the name (app.css).
  const button = (label, onClick, extra = {}, icon = null) => html`
    <button class="button" type="button" disabled=${busy} title=${icon ? label : null} onClick=${onClick} ...${extra}>
      ${icon && html`<${Icon} name=${icon} />`}<span class="button-words">${label}</span></button>`;
  // ^ The story above says what each action is for; the row holds the buttons alone.
  const buttons = acting ? [
    ...detail.approvals.map((request) => html`
      ${detail.approvals.length > 1 && html`<p class="task-approval">${request.label}</p>`}
      ${button("Deny", () => act.run("deny", `api/tasks/${project}/approvals/${request.id}/deny`, {}, null, request.label), { "aria-label": `Deny: ${request.label}` }, "fail")}
      ${button("Approve", () => act.run("approve", `api/tasks/${project}/approvals/${request.id}/approve`, {}, null, request.label), { "aria-label": request.label }, "pass")}`),
    // ^ A trial: this revision on the live site before it lands (docs/tasks.md "Hold and trial").
    // Pass lands it; a message to the worker fails it; End only frees the live site.
    trial && html`<p class="task-approval" role="status">${TRIAL_WORDS[trial.stage]}</p>`,
    trial?.stage === "live" && actions.includes("land") && button(act.busy === "pass" ? "Landing…" : "Pass and land", () => act.run("pass", `api/tasks/${project}/t/${task}/pass`, {}), {}, "pass"),
    trial && button(act.busy === "trial_end" ? "Ending…" : "End trial", () => act.run("trial_end", `api/tasks/${project}/t/${task}/trial_end`, {})),
    !trial && actions.includes("land") && button(act.busy === "land" ? "Landing…" : "Land", () => act.run("land", `api/tasks/${project}/t/${task}/land`, {}), {}, "land"),
    // ^ By hand: the owner's own session in the task's copy, with the worker's conversation.
    // ^ Its name says what happens; the line under the buttons says the rest (docs/web-client-tasks.md "Task sheet").
    actions.includes("hand") && button(act.busy === "hand" ? "Taking over…" : "Take over", () => act.run("hand", `api/tasks/${project}/t/${task}/hand`, {}),
      { "aria-describedby": "hand-hint" }),
    actions.includes("handback") && button(act.busy === "handback" ? "Handing back…" : "Hand back", () => act.run("handback", `api/tasks/${project}/t/${task}/handback`, {}),
      { "aria-describedby": "hand-hint" }),
    store.tasks[project]?.preview && detail.state !== "closed" && html`<${TryButton} project=${project} task=${task} />`,
  ].filter(Boolean) : [];
  // ^ The main checkout's changed files this land would overwrite, said before the press
  // (docs/tasks.md "Landing"); Land stays, it refuses until they are committed or stashed.
  const blocked = acting && actions.includes("land") && detail.land_blocked_by?.length > 0 && html`
    <p class="task-approval" role="note">Land stops: ${detail.default_branch} has uncommitted changes in ${detail.land_blocked_by.join(", ")}. Commit or stash them in the main checkout first.</p>`;
  const held = detail.hold && html`<p class="task-approval">Held for you to try: ${detail.hold}</p>`;
  const handHint = acting && (actions.includes("hand") || actions.includes("handback")) && html`<p class="task-approval task-hint" id="hand-hint">${actions.includes("hand")
    ? "Take over: the worker stops, and you continue in its working copy with its conversation in your own terminal. Hand back gives the task back to the worker, which checks your work."
    : "You work on this task in its working copy. Hand back gives it back to the worker, which checks your work."}</p>`;
  const report = sections(detail.report);
  const entries = detail.timeline.entries;
  // ^ Open items are the owner's to act on, so they stay outside Details.
  const toCheck = detail.to_check.some((item) => item.state === "open");
  const folds = FOLDS.filter((title) => report[title] != null && !(toCheck && title === "To check"));
  const lastResult = !summaryIsCurrent(detail) && detail.result?.summary?.trim();
  const checkRows = html`
    <ul class="check-list">${detail.to_check.map((item) => html`
      <${CheckRow} key=${item.id} project=${project} item=${item} revision=${detail.revision} onChanged=${load} />`)}
    </ul>`;
  return html`
    <div class="page task-page">
      ${top}
      <div class="scroll">
        <${TaskStory} detail=${detail} words=${words}>${held}${buttons.length ? html`
          <div class="task-next" role="group" aria-label="Actions">
            <div class="actions">${buttons}</div>
            ${blocked}
          </div>` : null}${handHint}</${TaskStory}>

        ${toCheck && html`
          <section class="task-section" aria-label="To check">
            <h2 class="task-heading">To check<span class="fold-note"> · ${foldNote("To check", "", detail)}</span></h2>
            ${checkRows}
          </section>`}
        <details class="task-section task-details">
          <summary class="task-heading fold-head"><${Icon} name="chevron" />Details</summary>
        ${lastResult && html`
          <section class="task-section" aria-label="Last result">
            <h2 class="task-heading">Last result</h2>
            <div class="task-body"><${Markdown} text=${lastResult} words=${words} prose /></div>
          </section>`}
        ${folds.map((title) => {
          const note = foldNote(title, report[title], detail);
          // ^ Landed items are live: Pass and Fail act on the project's list.
          const live = title === "To check" && detail.to_check.length;
          return html`
            <details class="task-section task-fold" key=${title}>
              <summary class="task-heading fold-head"><${Icon} name="chevron" />${title}${note && html`<span class="fold-note"> · ${note}</span>`}</summary>
              ${live ? checkRows
                : html`<div class="task-body"><${Markdown} text=${foldBody(title, report[title])} words=${words} prose /></div>`}
            </details>`;
        })}
        ${detail.talk.length ? html`<${Talk} project=${project} talk=${detail.talk} words=${words} />` : null}
        ${detail.conflict && detail.overlaps.length ? html`
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
        ${detail.usage?.turns > 0 && html`<p class="note task-usage">Usage: ${usageLine(detail.usage)}</p>`}
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
        </details>
      </div>
      ${act.error && html`<p class="error chat-error" role="alert">${act.error}</p>`}
      ${acting && actions.includes("add") && html`<${Message} detail=${detail} act=${act} />`}
    </div>`;
}
