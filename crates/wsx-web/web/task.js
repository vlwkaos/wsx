// One task, in its sheet over the project chat: what it is about, where it stands, what it does now
// and the one action when it needs the owner, the To check items to act on, then three folds at one
// level: the report, the messages to the worker and the history (docs/web-client-tasks.md "Task sheet").
import { api } from "./api.js";
import { clock, html, Icon, Loading, plural, short, useCoalescedLoad, useEffect, useState } from "./lib.js";
import { can, useEventLoad, useStore } from "./store.js";
import { Markdown } from "./markdown.js";
import { act } from "./task-api.js";
import { agentWords, taskStage, TaskChip, TaskTitle, withChips } from "./task-words.js";
import { Bubble } from "./bubbles.js";
import { quoteSplit } from "./reply.js";
import { SheetBar } from "./ui.js";
import { TaskStory } from "./story.js";
import { CheckRow, TryButton } from "./checks.js";
import { usageLine } from "./usage.js";

const TRIAL_WORDS = {
  preparing: "Going live: rebasing onto the default branch and checking",
  deploying: "Going live: deploying",
  live: "On the live site now: try it, then Pass, or tell the manager what is wrong",
};

const MESSAGE_WORDS = { prompt: "Asked", steer: "Steered", add: "Added" };

// One log entry in words: [what, detail, tone].
function described(entry) {
  switch (entry.kind) {
    case "created": return [`Created on ${entry.branch}`, `from ${short(entry.base)}`];
    case "message": return [MESSAGE_WORDS[entry.mode] ?? "Message", entry.text];
    case "steps": return [plural(entry.count, "step"), entry.last];
    case "blocked": return ["Blocked", entry.denials.length ? `denied: ${entry.denials.join(", ")}` : "", "attention"];
    case "result": return [`Result: ${entry.status}`, entry.summary, entry.status === "done" ? null : "danger"];
    case "checks": {
      const failed = entry.results.filter((check) => check.exit !== 0);
      return [`Checks: ${entry.results.length - failed.length} of ${entry.results.length} passed`,
        failed.map((check) => `${check.command}${check.timed_out ? " (timed out)" : ` (exit ${check.exit})`}`).join(", "), failed.length ? "danger" : null];
    }
    case "check_retry": return [`Checks failed on r${entry.revision}: sent back to the worker`,
      `${entry.commands.join(", ")} (automatic retry ${entry.attempt} of 2)`, "attention"];
    case "revision": return [`Revision r${entry.revision}`, `${short(entry.commit)} · ${plural(entry.paths_total, "file")}`];
    case "stopped": return ["Stopped", entry.reason];
    // ^ The host's keep-warm turn during a wait: no step, no result (docs/tasks.md "Context rotation").
    case "ping": return ["Ping", "kept the prompt cache warm during the wait"];
    case "closed": return [entry.discarded ? "Closed and discarded" : "Closed", ""];
    case "conflict": return [`Conflict onto ${short(entry.onto)}`, entry.paths.join(", "), "danger"];
    case "land_stopped": return ["Landing stopped", entry.reason, "danger"];
    case "landed": return ["Landed", short(entry.commit), "accent"];
    case "hand": return ["Worked by hand", entry.dropped ? `${plural(entry.dropped, "queued message")} dropped` : ""];
    case "handback": return ["Handed back", `${plural(entry.commits, "commit")}`];
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

// The report's parts, in the order of report.md after its summary; sections of older reports
// (Asked, Decisions, Human Verify) are not shown. They are headings in the one Report fold.
const PARTS = ["Changes", "Evidence", "To check", "Left open"];
const items = (body) => body.split("\n").filter((line) => line.startsWith("- ")).length;
// A part's text: Evidence without its first line (the fold's head says it); To check items before
// the landing as plain items, not checkboxes nobody can tick.
const partBody = (title, body) => title === "Evidence" ? body.split("\n").slice(1).join("\n").trim()
  : title === "To check" ? body.replace(/^- \[ \] /gm, "- ") : body;
// What To check says beside its heading: "2 open", "all checked", "1 after it lands".
function checkNote(body, detail) {
  const open = detail.to_check.filter((item) => item.state === "open").length;
  if (detail.to_check.length) return open ? `${open} open` : "all checked";
  return items(body) ? `${items(body)} after it lands` : "nothing";
}
// The Report fold's head: the checks' line and the open items left: "4 checks passed · 2 left open".
function reportNote(report) {
  const checks = report.Evidence?.split("\n")[0].replace(/\.$/, "").replace(/ \(`tasks.checks`\)$/, "");
  const left = report["Left open"] ? items(report["Left open"]) : 0;
  return [checks, left && `${left} left open`].filter(Boolean).join(" · ");
}

// An action's error, kept while the page re-renders.
function useAction(project, task) {
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const run = async (name, item = { task }) => {
    setBusy(name);
    setError("");
    try {
      await act(project, item, name);
    } catch (failure) {
      setError(failure.message);
    }
    setBusy("");
  };
  return { busy, error, run };
}

// The messages added or steered to the worker after its prompt, as bubbles.
function Talk({ project, talk, words }) {
  const rowOf = (entry, index) => ({
    kind: "message", key: `talk-${index}`, at: entry.unix_ms ?? 0, author: "owner", name: "You", ...quoteSplit(entry.text),
    first: index === 0, last: index === talk.length - 1,
  });
  return html`
    <section class="task-talk" aria-label="Messages">
      ${talk.map((entry, index) => html`<${Bubble} key=${index} project=${project} row=${rowOf(entry, index)} words=${words} />`)}
    </section>`;
}

// ^ The last read of each task this page made, so a sheet opens on its content at once and
// reads again behind it; the open decision list reads its tasks ahead (docs/web-client-tasks.md
// "Task sheet"). Kept for the page's life, at most DETAILS_KEPT, oldest out first.
const details = new Map();
const DETAILS_KEPT = 40;
const STATUS_EVERY_MS = 60_000;
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

export function TaskPage({ project, task, onClose, onMention, grip }) {
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
  // ^ A wait's sentence counts its minutes, and a wait raises no event: read it again each minute.
  const since = detail?.status?.since_unix_ms;
  useEffect(() => {
    if (since == null) return undefined;
    const timer = setInterval(load, STATUS_EVERY_MS);
    return () => clearInterval(timer);
  }, [since]);
  const action = useAction(project, task);
  const words = withChips(project);

  const top = html`
    <${SheetBar} grip=${grip} onClose=${onClose}>
      ${detail?.title ? html`<${TaskTitle} title=${detail.title} id=${task} />` : html`<span class="bar-label">#${task}</span>`}
      ${detail && !detail.progress && html`<span class="bar-state" data-tone=${taskStage(detail)[1]}>${taskStage(detail)[0]}</span>`}
      ${detail && !detail.progress && agentWords(detail.agent) && html`<span class="bar-model" title="Model">${agentWords(detail.agent)}</span>`}
    </${SheetBar}>`;
  if (!detail) return html`<div class="page">${top}${failure ? html`<p class="error empty">${failure}</p>` : html`<${Loading} />`}</div>`;

  const actions = detail.actions;
  const trial = detail.trial;
  const acting = can("task_act");
  const busy = Boolean(action.busy);
  const approval = (request) => ({ kind: "approval", approval: request.id, label: request.label });
  // ^ With an icon, a phone shows the icon alone and its words stay the name (app.css).
  const button = (label, onClick, extra = {}, icon = null) => html`
    <button class="button" type="button" disabled=${busy} title=${icon ? label : null} onClick=${onClick} ...${extra}>
      ${icon && html`<${Icon} name=${icon} />`}<span class="button-words">${label}</span></button>`;
  // ^ The story above says what each action is for; the row holds the buttons alone.
  const buttons = acting ? [
    ...detail.approvals.map((request) => html`
      ${detail.approvals.length > 1 && html`<p class="task-approval">${request.label}</p>`}
      ${button("Deny", () => action.run("deny", approval(request)), { "aria-label": `Deny: ${request.label}` }, "fail")}
      ${button("Approve", () => action.run("approve", approval(request)), { "aria-label": request.label }, "pass")}`),
    // ^ A trial: this revision on the live site before it lands (docs/tasks.md "Hold and trial").
    // Pass lands it; a message to the worker, through the manager, fails it; End only frees the live site.
    trial && html`<p class="task-approval" role="status">${TRIAL_WORDS[trial.stage]}</p>`,
    trial?.stage === "live" && actions.includes("land") && button(action.busy === "pass" ? "Landing…" : "Pass and land", () => action.run("pass"), {}, "pass"),
    trial && button(action.busy === "trial_end" ? "Ending…" : "End trial", () => action.run("trial_end")),
    // ^ The owner talks to the manager: the composer gets the mention, and the dialog closes in
    // the tap, since a modal dialog makes the composer inert and a phone raises its keyboard
    // only for a focus in the tap (docs/web-client-tasks.md "Task sheet").
    (detail.waiting?.on === "owner" || detail.state === "failed") && button("Answer", (event) => {
      event.currentTarget.closest("dialog").close();
      onMention(task);
    }, { "aria-label": `Answer #${task}: mention it to the manager` }, "reply"),
    !trial && actions.includes("land") && button(action.busy === "land" ? "Landing…" : "Land", () => action.run("land"), {}, "land"),
    // ^ By hand: the owner's own session in the task's copy, with the worker's conversation.
    // ^ Its name says what happens; the line under the buttons says the rest (docs/web-client-tasks.md "Task sheet").
    actions.includes("hand") && button(action.busy === "hand" ? "Taking over…" : "Take over", () => action.run("hand"), { "aria-describedby": "hand-hint" }),
    actions.includes("handback") && button(action.busy === "handback" ? "Handing back…" : "Hand back", () => action.run("handback")),
    store.tasks[project]?.preview && detail.state !== "closed" && html`<${TryButton} project=${project} task=${task} />`,
  ].filter(Boolean) : [];
  // ^ The main checkout's changed files this land would overwrite, said before the press
  // (docs/tasks.md "Landing"); Land stays, it refuses until they are committed or stashed.
  const blocked = acting && actions.includes("land") && detail.land_blocked_by?.length > 0 && html`
    <p class="task-approval" role="note">Land stops: ${detail.default_branch} has uncommitted changes in ${detail.land_blocked_by.join(", ")}. Commit or stash them in the main checkout first.</p>`;
  const held = detail.hold && html`<p class="task-approval">Held: ${detail.hold}</p>`;
  const handHint = acting && actions.includes("hand") && html`<p class="task-approval task-hint" id="hand-hint">Stops the worker. You continue in its session.</p>`;
  const report = sections(detail.report);
  const entries = detail.timeline.entries;
  // ^ Open items are the owner's to act on, so they stay outside the folds.
  const toCheck = detail.to_check.some((item) => item.state === "open");
  const parts = PARTS.filter((title) => report[title] != null && !(toCheck && title === "To check"));
  const reportNoted = reportNote(report);
  const checkRows = html`
    <ul class="check-list">${detail.to_check.map((item) => html`
      <${CheckRow} key=${item.id} project=${project} item=${item} revision=${detail.revision} onChanged=${load} onMention=${onMention} />`)}
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
            <h2 class="task-heading">To check<span class="fold-note"> · ${checkNote("", detail)}</span></h2>
            ${checkRows}
          </section>`}
        ${detail.status.waited_by?.length ? html`
          <section class="task-section" aria-label="Waited for by">
            <h2 class="task-heading">Waited for by</h2>
            <ul class="task-overlaps">
              ${detail.status.waited_by.map((waiter) => html`<li key=${waiter.task}><${TaskChip} project=${project} id=${waiter.task} /></li>`)}
            </ul>
          </section>` : null}
        ${detail.conflict && detail.overlaps.length ? html`
          <section class="task-section" aria-label="Shared files">
            <h2 class="task-heading">Shared files</h2>
            <ul class="task-overlaps">
              ${detail.overlaps.map((overlap) => html`
                <li key=${overlap.id}>
                  <${TaskChip} project=${project} id=${overlap.id} />
                  ${" "}${plural(overlap.count, "path")}${overlap.complete ? "" : " or more"}: <span class="path">${overlap.paths.join(", ")}</span>
                </li>`)}
            </ul>
          </section>` : null}
        ${parts.length > 0 && html`
          <details class="task-section task-fold">
            <summary class="task-heading fold-head"><${Icon} name="chevron" />Report${reportNoted && html`<span class="fold-note"> · ${reportNoted}</span>`}</summary>
            ${parts.map((title) => {
              // ^ Landed items are live: Pass and Fail act on the project's list.
              const live = title === "To check" && detail.to_check.length;
              return html`
                <section class="task-part" key=${title} aria-label=${title}>
                  <h3 class="task-heading">${title}${title === "To check" && html`<span class="fold-note"> · ${checkNote(report[title], detail)}</span>`}</h3>
                  ${live ? checkRows
                    : html`<div class="task-body"><${Markdown} text=${partBody(title, report[title])} words=${words} prose /></div>`}
                </section>`;
            })}
          </details>`}
        ${detail.talk.length ? html`
          <details class="task-section task-fold">
            <summary class="task-heading fold-head"><${Icon} name="chevron" />Messages<span class="fold-note"> · ${detail.talk.length}</span></summary>
            <${Talk} project=${project} talk=${detail.talk} words=${words} />
          </details>` : null}
        <details class="task-section timeline">
          <summary class="task-heading fold-head"><${Icon} name="chevron" />History</summary>
          ${detail.usage?.turns > 0 && html`<p class="note task-usage">Usage: ${usageLine(detail.usage)}</p>`}
          ${detail.timeline.skipped ? html`<p class="note">${detail.timeline.skipped} earlier entries are not shown.</p>` : null}
          <ol class="timeline-list">
            ${entries.map((entry, index) => {
              const [what, more, tone] = described(entry);
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
      ${action.error && html`<p class="error chat-error" role="alert">${action.error}</p>`}
    </div>`;
}
