// Manager mode on a project: the manager chat as messages with short task chips, what needs
// the owner and what runs in pills in the composer row that open lists over it, the `#` task picker, and a
// task's or the project overview's sheet over the chat (docs/web-client-tasks.md). Fixed actions
// go straight to wsx.
import { api } from "./api.js";
import { html, Icon, keepIn, plural, readMap, unreadNews, useCoalescedLoad, useEffect, useFollowEnd, useLayoutEffect, useNow, useRef, useState } from "./lib.js";
import { can, goBack, leaveFor, shows, state, touch, useEventLoad, useStore } from "./store.js";
import { Back, MoreMenu, toast } from "./ui.js";
import { AttachButton, FileChips, grow, isolate, JumpButton, pasteFiles, release, Suggestions, useDraft, useListKeys, useRecent, withNewLine } from "./chat.js";
import { Bubble, CheckBubble, conversation, Dots, Fold, QuietReply, StatusLine, Typing } from "./bubbles.js";
import { FindBar, FindButton } from "./find.js";
import { markFound, QuoteChip, quoteSplit, useReply, withQuote } from "./reply.js";
import { readTaskAhead, TaskPage } from "./task.js";
import { ProjectOverview } from "./overview.js";
import { ChecksPage, decideCheck } from "./checks.js";
import { UsagePage } from "./usage.js";
import { RoutineActions, routineAct, routineCounts, routineRow, routineWords, useRoutines } from "./routines.js";
import { PanelsButton, PanelsPage } from "./panels.js";
import { loadTasks, act } from "./task-api.js";
import { agentWords, calm, KIND_TONE, LEAVE_MS, running, taskLink, taskStage, TaskChip, withChips } from "./task-words.js";

// A count that ticks: it is a new element when it changes, which plays its entry.
const Tick = ({ value }) => html`<span class="tick" key=${value}>${value}</span>`;

// `list` with the items that just left kept in place for LEAVE_MS, to play their exit:
// `[{ item, key, leaving }]`.
function useLeaving(list, keyOf) {
  const [ghosts, setGhosts] = useState([]);
  const last = useRef(list);
  useEffect(() => {
    const keys = new Set(list.map(keyOf));
    const gone = calm() ? [] : last.current.filter((item) => !keys.has(keyOf(item))).map((item) => ({ item, key: keyOf(item), at: last.current.indexOf(item), leaving: true }));
    last.current = list;
    setGhosts((now) => {
      const kept = now.filter((entry) => !keys.has(entry.key));
      return kept.length === now.length && !gone.length ? now : [...kept, ...gone];
    });
    if (gone.length) setTimeout(() => setGhosts((now) => now.filter((entry) => !gone.includes(entry))), LEAVE_MS);
  }, [list.map(keyOf).join()]);
  const rows = list.map((item) => ({ item, key: keyOf(item), leaving: false }));
  for (const ghost of ghosts) rows.splice(Math.min(ghost.at, rows.length), 0, ghost);
  return rows;
}

const sameFields = (a, b) => {
  const keys = Object.keys(a);
  return keys.length === Object.keys(b).length && keys.every((key) => a[key] === b[key]);
};

const itemKey = (item) => item.kind === "approval" ? item.approval : item.kind === "ask" ? `ask-${item.id}` : `${item.kind}-${item.task}`;

// Outcomes of actions, by `<project>/<item>`, kept across the cards' re-renders.
const pending = new Map();
const actionOf = (key) => {
  if (!pending.has(key)) pending.set(key, { busy: false, error: "" });
  return pending.get(key);
};

// One row of any of the four lists, the same in each: the task's chip (its number and state
// dot) or, for a row without a task, a tone dot; the label and the context under it (one line,
// or several); the row's actions. The text opens `href`, or `onOpen` when it is a button.
function ListRow({ rowKey, leaving, project, task = null, tone, label, context, href, onOpen, openLabel, actions, error }) {
  const body = html`<span class="row-label">${label}</span>${[context].flat().map((line, at) => line && html`<span class="row-context" key=${at}>${line}</span>`)}`;
  return html`
    <li class="need" data-key=${rowKey} data-leaving=${leaving ? "" : null} inert=${Boolean(leaving)}>
      <div class="row-link">
        ${task != null ? html`<${TaskChip} project=${project} id=${task} plain />` : html`<span class="mark" data-tone=${tone} aria-hidden="true"></span>`}
        ${href ? html`<a class="row-main row-open" href=${href}>${body}</a>`
          : onOpen ? html`<button class="row-main row-open" type="button" aria-label=${openLabel} onClick=${onOpen}>${body}</button>`
          : html`<span class="row-main">${body}</span>`}
        ${actions && html`<span class="row-actions">${actions}</span>`}
      </div>
      ${error && html`<p class="error need-error" role="alert">${error}</p>`}
    </li>`;
}

// ^ Icons, so the title and what the item waits for keep the row's width; the name is the
// button's title and, with what it acts on, its accessible name.
const rowButton = (busy, label, icon, tone, onClick, extra = {}) => html`
  <button class="button icon-button need-action" type="button" title=${label} data-tone=${tone} disabled=${busy}
    onClick=${onClick} ...${extra}><${Icon} name=${icon} /></button>`;

// The state of the action a row runs, kept across renders: `run` starts it, `onActing` says an
// action started and `onFailed` that it failed (the reservoir moves the focus on when the row
// leaves, before or after the action's answer).
function useRowAction(key, id, index, onActing, onFailed) {
  const status = actionOf(key);
  const [, renew] = useState(0);
  const refresh = () => renew((count) => count + 1);
  const run = async (action) => {
    status.busy = true;
    status.error = "";
    refresh();
    onActing(id, index);
    try {
      await action();
    } catch (failure) {
      onFailed(id);
      status.error = failure.message;
    }
    status.busy = false;
    refresh();
  };
  return [status, run, refresh];
}

// One open item in the reservoir: its task chip, its title (an approval has no chip: its label
// names its task) and what it waits for, with its one action. The title opens the task's sheet, where the
// other actions are.
// `onMention` puts the task's number into the manager's composer: the owner talks to the
// manager, which relays to the worker (docs/web-client-tasks.md "Project screen"). An open
// question has Reply (`onReply` quotes it in the composer) and Dismiss; its label and "Asks you"
// are one button that takes the chat to the reply (`onShow` with its message id).
function NeedRow({ project, item, index, onActing, onFailed, onMention, onReply, onShow, leaving }) {
  const [status, doAct, refresh] = useRowAction(`${project}/${itemKey(item)}`, itemKey(item), index, onActing, onFailed);
  const acting = can("task_act");
  const overview = useStore().tasks[project];
  // ^ This task's trial on the live site (docs/tasks.md "Hold and trial").
  const trial = overview?.trial?.task === item.task ? overview.trial : null;
  const run = (action) => doAct(() => act(project, item, action));
  const button = (...args) => rowButton(status.busy, ...args);
  let actions = null;
  if (acting && item.kind === "ask") {
    actions = html`${button("Reply", "reply", null, () => onReply(item.label, item.id), { "aria-label": `Reply: ${item.label}` })}${button("Dismiss", "fail", null, () => run("dismiss"), { "aria-label": `Dismiss: ${item.label}` })}`;
  } else if (acting && ["waiting", "failed"].includes(item.kind)) {
    actions = button("Answer", "reply", null, () => onMention(item.task), { "aria-label": `Answer #${item.task}: mention it to the manager` });
  } else if (acting && item.kind === "approval" && status.running) {
    // ^ The approved action runs until its decision settles the row; it may take minutes.
    actions = html`<span class="row-context" role="status">Running…</span>`;
  } else if (acting && item.kind === "approval") {
    const approve = async () => {
      await run("approve");
      if (!status.error) { status.running = true; refresh(); }
    };
    actions = html`${button("Deny", "fail", "danger", () => run("deny"), { "aria-label": `Deny: ${item.label}` })}${button("Approve", "pass", "accent", approve, { "aria-label": item.label })}`;
  } else if (acting && item.kind === "land" && trial?.stage === "live") {
    actions = html`${button("Fail: tell the manager", "reply", "danger", () => onMention(item.task), { "aria-label": `Fail #${item.task}: tell the manager what is wrong` })}${button("Pass and land", "pass", "accent", () => run("pass"), { "aria-label": `Pass #${item.task} and land it` })}`;
  } else if (acting && item.kind === "land" && trial) {
    actions = html`<span class="row-context" role="status">Going live…</span>`;
  } else if (acting && item.kind === "land") {
    actions = button("Land", "land", "accent", () => run("land"), { "aria-label": `Land #${item.task}` });
  }
  // ^ An approval reads as the action it asks for, never as its id (docs/web-client-tasks.md).
  const context = item.kind === "ask" ? "Asks you" : item.kind === "approval" ? item.title ?? item.url ?? item.commit ?? ""
    : item.kind === "land" && trial?.stage === "live" ? "on the live site: try it" : item.status.label;
  const asks = item.kind === "ask";
  return html`
    <${ListRow} rowKey=${itemKey(item)} leaving=${leaving} project=${project} task=${item.kind === "approval" ? null : item.task} tone=${KIND_TONE[item.kind]}
      label=${asks || item.kind === "approval" ? item.label : item.title} context=${context}
      href=${item.task != null ? taskLink(project, item.task) : null}
      onOpen=${asks ? () => onShow(item.id) : null} openLabel=${`Show the question: ${item.label}`}
      actions=${actions} error=${status.error} />`;
}

// A working task in the reservoir: its chip, its title, and its status, wait and agent.
const RunningRow = ({ project, task, leaving }) => html`
  <${ListRow} rowKey=${`run-${task.id}`} leaving=${leaving} project=${project} task=${task.id} label=${task.title}
    context=${[task.status.label, task.status.wait, agentWords(task.agent)].filter(Boolean).join(" · ")}
    href=${taskLink(project, task.id)} />`;

// An open To check item: its check, what passes it, Fail (tells the manager) and Pass as the
// sheet has them (checks.js `CheckRow`). The text opens the sheet at the item.
function CheckingRow({ project, item, index, onActing, onFailed, onMention, leaving }) {
  const [status, run] = useRowAction(`${project}/check-${item.id}`, item.id, index, onActing, onFailed);
  const button = (...args) => rowButton(status.busy, ...args);
  const actions = can("task_act") && html`
    ${button("Fail: tell the manager", "fail", "danger", () => onMention(item.task, `To check ${item.id} failed: `), { "aria-label": `Fail: tell the manager what is wrong with: ${item.check}` })}
    ${button("Works: close this check", "pass", "accent", () => run(() => decideCheck(project, item.id, "pass")), { "aria-label": `Pass, works: close this check: ${item.check}` })}`;
  return html`
    <${ListRow} rowKey=${item.id} leaving=${leaving} project=${project} task=${item.task} label=${item.check}
      context=${`Passes if ${item.pass_if}`} href=${`#/p/${project}/checks/${item.id}`} actions=${actions} error=${status.error} />`;
}

// The four lists over the composer and the pills that open them (docs/web-client-tasks.md
// "Project screen"). Each pill is an icon and its count, in the composer row; a count of 0 has no
// pill, except Routines, which shows while the project has any routine (its count is the enabled
// ones: `counts.routines` is the `routineCounts` of the project). One list is open at a time.
const TRAYS = [
  { key: "running", icon: "play", label: "Running", count: (c) => c.running, words: () => "running" },
  { key: "needs", icon: "unchecked", label: "Needs you", count: (c) => c.needs, words: (count) => count === 1 ? "needs you" : "need you" },
  { key: "check", icon: "pass", label: "To check", count: (c) => c.check, words: () => "to check" },
  { key: "routines", icon: "routines", label: "Routines", count: (c) => c.routines.on, shown: (c) => c.routines.total > 0, words: (count, c) => routineWords(c.routines), tone: (c) => c.routines.tone },
];
const present = (counts, tray) => tray.shown ? tray.shown(counts) : tray.count(counts) > 0;

// The open list's name, or null: a list whose count fell to 0 closes. A tap on the open pill
// closes it; a tap on another pill switches to its list.
function useTray(counts) {
  const [tray, setTray] = useState(null);
  const open = tray && present(counts, TRAYS.find((each) => each.key === tray)) ? tray : null;
  useEffect(() => { if (tray && !open) setTray(null); }, [tray, open]);
  return [open, (key) => setTray(open === key ? null : key), setTray];
}

// `bar`: a device that cannot act has no composer, so the pills stand in a bar of their own.
function Pills({ counts, open, onToggle, bar = false }) {
  const shown = TRAYS.filter((tray) => present(counts, tray));
  if (!shown.length) return null;
  return html`
    <div class=${bar ? "pills pill-bar" : "pills"} role="group" aria-label="Running, needs you, to check, routines">
      ${shown.map((tray) => html`
        <button class="pill" type="button" key=${tray.key} data-kind=${tray.key} data-tone=${tray.tone?.(counts) ?? null} aria-pressed=${open === tray.key}
          aria-controls=${open === tray.key ? "reservoir-list" : null} aria-label=${tray.key === "routines" ? tray.words(0, counts) : `${tray.count(counts)} ${tray.words(tray.count(counts), counts)}`}
          onClick=${() => onToggle(tray.key)}>
          <${Icon} name=${tray.icon} /><span class="pill-count"><${Tick} value=${tray.count(counts)} /></span><span class="pill-label" aria-hidden="true">${tray.label}</span>
        </button>`)}
    </div>`;
}

// A routine in the Routines list: the schedule, the last run and whether it has a message. A
// tap on the text of a routine with a message takes the chat to it (`onShow`); the actions turn
// the routine on or off and run it now, for a device that may act. `onList` takes the list the
// action answers.
function RoutineRow({ project, row, now, onList, onShow }) {
  const [status, doAct] = useRowAction(`${project}/routine-${row.name}`, row.name, 0, () => {}, () => {});
  const { tone, message, context } = routineRow(row, now);
  const actions = can("task_act") && html`<${RoutineActions} row=${row} busy=${status.busy} onAct=${(verb) => doAct(async () => onList(await routineAct(project, row.name, verb)))} />`;
  return html`
    <${ListRow} rowKey=${`routine-${row.name}`} project=${project} tone=${tone} label=${row.name} context=${context}
      onOpen=${message != null ? () => onShow(message) : null} openLabel=${`Show the message of ${row.name}'s last run`}
      actions=${actions} error=${status.error} />`;
}

// The open list, pinned over the composer: Running (the tasks that work), Needs you (every item
// that waits for the owner in the server's order, each with its one action), To check or Routines. Done
// tasks are in none of them. A cleared row leaves and the focus moves to the next row's action,
// or to the composer after the last one. Only a question's label moves the chat, to its reply.
function Reservoir({ project, open, items, tasks, checks, routines, onRoutines, look, onLooked, onMention, onReply, onShow, onClose }) {
  const node = useRef();
  // ^ A focus inside the list would fall to the body when the list goes: it goes to the open pill.
  const close = () => {
    if (node.current?.contains(document.activeElement)) document.querySelector(`.pill[data-kind=${open}]`)?.focus({ preventScroll: true });
    onClose();
  };
  // ^ The row whose action runs: a `tasks` event may remove it long before the answer comes.
  const cleared = useRef(null);
  const working = tasks.filter(running);
  const keys = open === "check" ? checks.map((item) => item.id) : items.map(itemKey);
  const needRows = useLeaving(items, itemKey);
  const runRows = useLeaving(working, (task) => `run-${task.id}`);
  const checkRows = useLeaving(checks, (item) => item.id);
  const now = useNow(open === "routines");
  useLayoutEffect(() => {
    const done = cleared.current;
    if (!done || keys.includes(done.key)) return;
    cleared.current = null;
    // ^ Only when the focus is still in the list, or left with the removed button.
    const focus = document.activeElement;
    if (focus && focus !== document.body && !node.current?.contains(focus)) return;
    const next = keys[Math.min(done.index, keys.length - 1)];
    const row = next && node.current?.querySelector(`.need[data-key="${CSS.escape(next)}"]`);
    // ^ The row's action first; its title only when the site may not act; the open pill last.
    const target = row?.querySelector(".need-action") ?? row?.querySelector(".row-open") ?? document.querySelector(".pill[aria-pressed=true]");
    target?.focus({ preventScroll: true });
  }, [keys.join()]);
  // ^ Escape closes the list from inside it, from the pills or from the page body (a text field keeps
  // its own Escape). The focus goes to the pill, never to a text field: that opens a phone's keyboard.
  useEffect(() => {
    if (!open) return;
    const onKey = (event) => {
      const at = event.target;
      if (event.key !== "Escape" || event.defaultPrevented || !(at === document.body || at.closest?.(".reservoir, .pills"))) return;
      event.preventDefault();
      close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);
  // ^ A tag under a reply asked for this row: it scrolls into view and shows marked, as a found message does.
  useLayoutEffect(() => {
    if (!look || open !== "needs") return;
    const row = node.current?.querySelector(`.need[data-key="${CSS.escape(look)}"]`);
    if (row) { row.scrollIntoView({ block: "nearest" }); markFound(row); }
    onLooked();
  }, [look, open]);
  // ^ Open, each item's task is read ahead, so its sheet shows at once.
  useEffect(() => {
    if (open === "needs") for (const item of items) if (item.task != null) readTaskAhead(project, item.task);
  }, [open, keys.join()]);
  if (!open) return null;
  const onActing = (key, index) => { cleared.current = { key, index }; };
  const onFailed = (key) => { if (cleared.current?.key === key) cleared.current = null; };
  const label = TRAYS.find((tray) => tray.key === open).label;
  return html`
    <section class="reservoir" ref=${node} aria-label=${label}>
      <div class="reservoir-reveal" key=${open}>
        <div class="reservoir-body" id="reservoir-list">
          <div class="reservoir-head">
            <span class="reservoir-title">${label}</span>
            <button class="icon-button reservoir-close" type="button" aria-label=${`Close ${label}`} title=${`Close ${label}`} onClick=${close}><${Icon} name="down" /></button>
          </div>
          <div class="reservoir-scroll">
            ${open === "running" ? html`
              <ul class="reservoir-list" aria-label="Running">
                ${runRows.map(({ item, key, leaving }) => html`<${RunningRow} key=${key} project=${project} task=${item} leaving=${leaving} />`)}
              </ul>` : null}
            ${open === "needs" ? html`
              <ul class="reservoir-list" aria-label="Needs you">
                ${needRows.map(({ item, key, leaving }) => html`<${NeedRow} key=${key} project=${project} item=${item} index=${items.indexOf(item)} onActing=${onActing} onFailed=${onFailed} onMention=${onMention} onReply=${onReply} onShow=${onShow} leaving=${leaving} />`)}
              </ul>` : null}
            ${open === "check" ? html`
              <ul class="reservoir-list" aria-label="To check">
                ${checkRows.map(({ item, key, leaving }) => html`<${CheckingRow} key=${key} project=${project} item=${item} index=${checks.indexOf(item)} onActing=${onActing} onFailed=${onFailed} onMention=${onMention} leaving=${leaving} />`)}
              </ul>` : null}
            ${open === "routines" ? html`
              ${routines.error ? html`<p class="error empty" role="alert">${routines.error}</p>` : null}
              <ul class="reservoir-list" aria-label="Routines">
                ${routines.routines.map((row) => html`<${RoutineRow} key=${row.name} project=${project} row=${row} now=${now} onList=${onRoutines} onShow=${onShow} />`)}
              </ul>` : null}
          </div>
        </div>
      </div>
    </section>`;
}

// Unsent manager messages by project, kept on this device.
const DRAFTS_KEY = "wsx-manager-drafts";

// While the word at the caret starts with `#` (a task), `@` (another project) or, as the whole
// text so far, `/` (one of the owner's skills): what it names so far.
function wordQuery(text, caret) {
  const before = text.slice(0, caret);
  const typed = /(?:^|\s)([#@])([\w.-]*)$/.exec(before) ?? /(?:^|\s)(\/)([\w:.-]*)$/.exec(before);
  return typed ? { mark: typed[1], query: typed[2].toLowerCase(), start: caret - typed[2].length - 1 } : null;
}
const within = (names, query, pick) => {
  const starts = names.filter((item) => pick(item).toLowerCase().startsWith(query));
  return [...starts, ...names.filter((item) => !starts.includes(item) && pick(item).toLowerCase().includes(query))].slice(0, 50);
};

// The owner's skills and the other projects, read once per project while the page is open.
const mentionLists = new Map();
function loadMentions(project) {
  if (!mentionLists.has(project)) {
    mentionLists.set(project, api(`api/tasks/${project}/mentions`).catch((failure) => {
      mentionLists.delete(project);
      throw failure;
    }));
  }
  return mentionLists.get(project);
}

const LIST_LABELS = { "#": "Tasks", "@": "Projects", "/": "Skills" };
const emptyNote = (mark, tasks, mentions, failed, retry) => mark === "#" ? (tasks.length ? "No task matches that." : "This project has no task yet.")
  : failed ? html`<span class="slash-failed" role="alert">${failed}</span>
    <button class="link-action" type="button" onPointerDown=${(event) => event.preventDefault()} onClick=${retry}>Retry</button>`
  : !mentions ? "Loading…"
  : mark === "@" ? (mentions.projects.length ? "No project matches that." : "There is no other project.")
  : (mentions.skills.length ? "No skill matches that." : "You have no skills to lend the manager.");

// The `#` picker's tasks: the title first, the number after it. A task done or closed more than
// a day ago folds away until the text after `#` names it (docs/web-client-tasks.md "Project screen").
const DAY_MS = 86_400_000;
const settled = (task) => ["done", "closed"].includes(task.stage) && Date.now() - task.updated_unix_ms > DAY_MS;
const taskOptions = (tasks, query) => tasks
  .filter((task) => (query || !settled(task)) && (String(task.id).startsWith(query) || task.title.toLowerCase().includes(query))).slice(0, 50)
  .map((task) => ({ key: task.id, name: task.title, number: `#${task.id}`, source: taskStage(task)[0], tone: taskStage(task)[1], insert: `#${task.id} ` }));
const foldedNote = (tasks, query) => {
  const folded = query ? 0 : tasks.filter(settled).length;
  return folded ? `${folded} done more than a day ago: type a number or a word to find ${folded === 1 ? "it" : "them"}.` : null;
};

// A draft with a mention of task `n`, the caret after it: [text, caret]. A new mention goes after
// the words; one already there stays where it is, and ends with a space so no `#` list opens.
function withMention(text, n) {
  const mark = `#${n}`;
  const found = text.search(new RegExp(`${mark}\\b`));
  if (found < 0) {
    const next = `${text}${text && !/\s$/.test(text) ? " " : ""}${mark} `;
    return [next, next.length];
  }
  const end = found + mark.length;
  if (end === text.length) return [`${text} `, end + 1];
  return [text, /\s/.test(text[end]) ? end + 1 : end];
}

function ManagerComposer({ project, role, busy: thinking, reply, onReply, onSent, field, pills }) {
  const store = useStore();
  // ^ A role's draft is its own.
  const draft = role ? `${project}/${role}` : project;
  const [text, setText] = useState(() => readMap(DRAFTS_KEY)[draft] ?? "");
  const [caret, setCaret] = useState(0);
  const { active, setActive, shut, keys } = useListKeys(text);
  const [busy, setBusy] = useState(false);
  // ^ Files to send with the next message; they are lost on leaving the project.
  const files = useRef({ attachments: [], uploads: [] }).current;
  const [, renew] = useState(0);
  const filesChanged = () => renew((count) => count + 1);
  const tasks = store.tasks[project]?.tasks ?? [];
  const typed = shut ? null : wordQuery(text, caret);
  const [mentions, setMentions] = useState(null);
  // ^ A failed read shows its error and a Retry in the list, not "Loading…" for ever.
  const [failed, setFailed] = useState("");
  const [attempt, setAttempt] = useState(0);
  const mark = typed?.mark;
  useEffect(() => {
    if (mark !== "/" && mark !== "@") return undefined;
    let current = true;
    setFailed("");
    loadMentions(project).then((value) => { if (current) setMentions(value); }, (failure) => { if (current) setFailed(failure.message); });
    return () => { current = false; };
  }, [project, mark === "/" || mark === "@", attempt]);
  // ^ `#` lists tasks, `@` the other projects, `/` the owner's skills the manager can use. A
  // skill is asked for in words: the manager is headless and takes no slash command.
  const options = !typed ? null
    : mark === "#" ? taskOptions(tasks, typed.query)
    : mark === "@" ? within(mentions?.projects ?? [], typed.query, (other) => other.name)
      .map((other) => ({ key: other.id, name: `@${other.name}`, description: other.manager ?? "", source: "project", insert: `@${other.name} ` }))
    : within(mentions?.skills ?? [], typed.query, (skill) => skill.name)
      .map((skill) => ({ key: skill.name, name: `/${skill.name}`, description: skill.description, source: "skill", insert: typed.start ? `owner:${skill.name} ` : `Use owner:${skill.name}: ` }));
  const listOpen = Boolean(options);
  useLayoutEffect(() => { if (listOpen) document.getElementById(`slash-${active}`)?.scrollIntoView({ block: "nearest" }); }, [active, listOpen]);
  const changed = () => {
    const node = field.current;
    setText(node.value);
    setCaret(node.selectionStart);
    setActive(0);
    keepIn(DRAFTS_KEY, draft, node.value);
    grow(node);
  };
  const { error, setError, upload, inline, remove, focusField } = useDraft(field, files, { text, key: project, filesChanged, inlined: () => { changed(); filesChanged(); } });
  const pick = ({ insert }) => {
    const node = field.current;
    node.value = text.slice(0, typed.start) + insert + text.slice(caret);
    const at = typed.start + insert.length;
    node.focus();
    node.setSelectionRange(at, at);
    changed();
  };
  const send = async () => {
    const body = field.current.value.trim();
    if (busy || files.uploads.length || (!body && !files.attachments.length)) return;
    setBusy(true);
    setError("");
    try {
      const attachments = files.attachments.map((item) => item.hash);
      const text = withQuote(reply?.line, body);
      // ^ A say may start the manager first: up to about 40 s (docs/tasks.md "Manager").
      const answer = await api(`api/tasks/${project}/say`, { method: "POST", json: { text: reply?.id != null ? `> message ${reply.id}\n\n${body}` : text, attachments, ...(role && { role }) }, timeout: 60_000 });
      field.current.value = "";
      files.attachments = [];
      changed();
      onReply(null);
      onSent({ id: answer.id, text });
    } catch (failure) {
      // The draft stays in the field; nothing was sent.
      setError(failure.message);
    }
    setBusy(false);
  };
  const keydown = (event) => {
    if (event.isComposing || event.keyCode === 229) return;
    if (listOpen && keys(event, options, pick)) return;
    if (event.key !== "Enter" || event.shiftKey || touch()) return;
    event.preventDefault();
    send();
  };
  return html`
    <div class="composer">
      <${FileChips} files=${files} onInline=${inline} onRemove=${remove} />
      ${error && html`<p class="error" role="alert">${error}</p>`}
      <${QuoteChip} quote=${reply?.line} onRemove=${() => onReply(null)} />
      ${listOpen && html`<${Suggestions} label=${LIST_LABELS[mark]} active=${active} onHover=${setActive} onPick=${pick}
        options=${options}
        notes=${[!options.length && emptyNote(mark, tasks, mentions, failed, () => setAttempt((count) => count + 1)), mark === "#" && foldedNote(tasks, typed.query)]} />`}
      <div class="composer-row" onPointerDown=${focusField}>
        <${AttachButton} onFiles=${upload} />
        ${pills}
        <div class="composer-input">
          <textarea class="composer-field" ref=${field} rows="1" aria-label=${`Message to ${role ?? "the manager"}`}
            placeholder="Message · # task @ project / skill" role="combobox" aria-autocomplete="list" aria-expanded=${listOpen}
            aria-controls=${listOpen ? "slash-list" : null} aria-activedescendant=${listOpen && options.length ? `slash-${active}` : null}
            onInput=${changed} onKeyDown=${keydown} onPaste=${pasteFiles(upload)} onKeyUp=${() => setCaret(field.current.selectionStart)}
            onClick=${() => setCaret(field.current.selectionStart)} onFocus=${isolate} onBlur=${release}></textarea>
        </div>
        <button class="button button-primary composer-send" type="button" aria-label=${thinking ? "Queue" : "Send"}
          disabled=${busy || files.uploads.length > 0} onClick=${send}>
          <span class="send-mark" aria-hidden="true"><${Icon} name="send" /></span><span class="send-text">${thinking ? "Queue" : "Send"}</span>
        </button>
      </div>
    </div>`;
}

// When this device last left each project's screen, for "since you left".
const LEFT_KEY = "wsx-project-left";
const readLeft = () => readMap(LEFT_KEY);
const noteLeft = (project) => keepIn(LEFT_KEY, project, Date.now());
// Shorter absences are a glance away, not an absence.
const AWAY_MS = 60_000;
// ^ The word leads each group, so a phone shows what happened before any ids.
const SINCE_WORDS = [["landed", "Landed"], ["ready", "Ready"], ["failed", "Failed"], ["blocked", "Blocked"], ["conflict", "Conflicted"], ["revised", "Revised"], ["started", "Started"]];
// The line never cuts what happened: it wraps to two lines, and when a group's ids would need more
// it shows the first ones and "+N more" (docs/web-client-tasks.md "Since you left").
const SINCE_LINES = 2;

// "Since you left": what the task logs say happened while this device was away from the
// project, on opening it or on coming back to it. Nothing runs while away. It goes by itself
// once the newest reply that came while away has been on screen, or the owner sent a message.
// `reply` is the time of the newest message that is not the owner's.
function SinceLine({ project, reply, sent }) {
  const [since, setSince] = useState(null);
  useEffect(() => {
    const ask = () => {
      const left = readLeft()[project];
      if (left && Date.now() - left > AWAY_MS) {
        api(`api/tasks/${project}/since`, { query: { after: left } }).then((data) => setSince({ ...data, left }), () => {});
      }
    };
    const changed = () => (document.visibilityState === "hidden" ? noteLeft(project) : ask());
    ask();
    document.addEventListener("visibilitychange", changed);
    return () => {
      document.removeEventListener("visibilitychange", changed);
      noteLeft(project);
    };
  }, [project]);
  useEffect(() => { if (sent) setSince(null); }, [sent]);
  // ids shown per group: all, then fewer, at last only the count, until the text fits SINCE_LINES
  // lines. The cap belongs to one answer (`for`), so a new answer starts with all ids.
  const [fit, setFit] = useState({ for: null, cap: null });
  const cap = fit.for === since ? fit.cap : null;
  const box = useRef(null);
  const width = useRef(0);
  useLayoutEffect(() => {
    const text = box.current?.querySelector("p");
    if (!text) return;
    const most = Math.max(0, ...Object.values(since.tasks).map((ids) => ids.length));
    const fits = text.scrollHeight <= SINCE_LINES * parseFloat(getComputedStyle(text).lineHeight) + 1;
    if (!fits && (cap ?? most) > 0) setFit({ for: since, cap: (cap ?? most) - 1 });
  }, [since, cap]);
  useLayoutEffect(() => {
    if (!box.current) return undefined;
    const watch = new ResizeObserver(([one]) => {
      const next = Math.round(one.contentRect.width);
      if (width.current && next !== width.current) setFit({ for: null, cap: null });
      width.current = next;
    });
    watch.observe(box.current);
    return () => watch.disconnect();
  }, [!since]);
  const left = since?.left;
  useEffect(() => {
    if (!left || !reply || reply <= left) return undefined;
    // ^ Bubbles of one entry share a time: the newest is the last of them.
    const bubble = [...document.querySelectorAll(`.log .bubble[data-side="in"][data-at="${reply}"]`)].at(-1);
    if (!bubble) return undefined;
    const watch = new IntersectionObserver((seen) => { if (seen.some((one) => one.isIntersecting)) setSince(null); }, { root: bubble.closest(".log") });
    watch.observe(bubble);
    return () => watch.disconnect();
  }, [left, reply]);
  if (!since) return null;
  const parts = SINCE_WORDS.filter(([key]) => since.tasks[key]?.length).map(([key, words]) => html`
    <span key=${key}>${words} ${cap === 0 ? since.tasks[key].length : html`${since.tasks[key].slice(0, cap ?? undefined).map((id) => html`<a href=${taskLink(project, id)}>#${id}</a> `)}${cap && since.tasks[key].length > cap ? html`<span class="since-more">+${since.tasks[key].length - cap} more</span>` : ""}`}</span>`);
  if (since.replies) parts.push(html`<span key="replies">${plural(since.replies, "reply", "replies")}</span>`);
  if (!parts.length) return null;
  return html`
    <div class="since" role="status" ref=${box}>
      <p>${parts.map((part, index) => html`${index ? " · " : ""}${part}`)}</p>
      <button class="icon-button" type="button" title="Dismiss" aria-label="Dismiss" onClick=${() => setSince(null)}><${Icon} name="fail" /></button>
    </div>`;
}

// The top row's ⋯: the project overview, sessions and files.
// The project's one shell in its main checkout, the one the TUI's `S` opens: the live one,
// else a new one (docs/web-client-tasks.md "Project screen").
function ProjectShell({ project }) {
  const [busy, setBusy] = useState(false);
  if (!shows("terminal") || !can("session_start") || !can("terminal_input")) return null;
  const open = async () => {
    setBusy(true);
    try {
      const { session_id } = await api(`api/projects/${project.id}/shell`, { method: "POST" });
      location.hash = `#/s/${session_id}/terminal`;
    } catch (failure) {
      toast(failure.message, { failure: true });
    }
    setBusy(false);
  };
  return html`<button class="icon-button project-shell" type="button" disabled=${busy} onClick=${open}
    aria-label="Shell" title="Shell"><${Icon} name="terminal" /></button>`;
}

function More({ project, roles }) {
  const files = shows("files") && can("files_read");
  return html`
    <${MoreMenu}>
      <a role="menuitem" href=${`#/p/${project.id}/sessions`}>Sessions</a>
      <a role="menuitem" href=${`#/p/${project.id}/usage`}>Usage</a>
      ${files && html`<a role="menuitem" href=${`#/p/${project.id}/files`}>Files</a>`}
      ${roles?.length > 0 && html`
        <p class="more-heading" aria-hidden="true">Roles</p>
        ${roles.map((role) => {
          const unread = Boolean(unreadNews(`project-${project.id}-r-${role.name}`, role.news));
          return html`<a role="menuitem" class="role-contact" key=${role.name} href=${`#/p/${project.id}/r/${encodeURIComponent(role.name)}`}
            aria-label=${`Role ${role.name}${unread ? ", unread" : ""}`}>
            <span class="role-name">${role.name}${unread && html`<span class="news-mark" aria-hidden="true"></span>`}</span>
            <span class="role-line">${role.last_reply ?? role.brief}</span>
          </a>`;
        })}`}
    </${MoreMenu}>`;
}

// A task or the project overview over the chat, about 85% of the height (a `fit` sheet only as
// high as its content): Back, Escape, a tap
// beside it or a swipe down on its top closes it, and the chat under it keeps its scroll and
// its draft. A fit sheet's top follows its height, so it opens once its content calls `ready`:
// rows that came after the slide would lift it again. Its first read gives up after a while
// (api.js), so `ready` always comes.
function Sheet({ label, fit = false, onClose, children }) {
  const dialog = useRef();
  const drag = useRef(null);
  const [ready, setReady] = useState(!fit);
  useEffect(() => {
    if (ready && !dialog.current.open) dialog.current.showModal();
  }, [ready]);
  useEffect(() => {
    const node = dialog.current;
    return () => { if (node.open) node.close(); };
  }, []);
  const cancel = (event) => { event.preventDefault(); onClose(); };
  const backdrop = (event) => { if (event.target === dialog.current) onClose(); };
  const down = (event) => {
    if (event.target.closest("button, a, input, textarea")) return;
    drag.current = { y: event.clientY, id: event.pointerId };
    event.currentTarget.setPointerCapture(event.pointerId);
  };
  const move = (event) => {
    if (drag.current?.id !== event.pointerId) return;
    dialog.current.style.translate = `0 ${Math.max(0, event.clientY - drag.current.y)}px`;
  };
  const up = (event) => {
    if (drag.current?.id !== event.pointerId) return;
    const moved = event.clientY - drag.current.y;
    drag.current = null;
    dialog.current.style.translate = "";
    if (moved > 80) onClose();
  };
  return html`
    <dialog class="task-sheet" data-fit=${fit || undefined} ref=${dialog} aria-label=${label} onCancel=${cancel} onClick=${backdrop}>
      <div class="task-sheet-top" onPointerDown=${down} onPointerMove=${move} onPointerUp=${up} onPointerCancel=${up}>
        <span class="sheet-grip" aria-hidden="true"></span>
      </div>
      ${children({ down, move, up, ready: () => setReady(true) })}
    </dialog>`;
}

// ^ The sheets over the chat, by the route's `sheet` (store.js parseRoute): the page, its label, `fit` (only
// as high as its content) and `refocus` (closing it gives the composer the focus). A page takes the
// props `project`, `task`, `item`, `onClose`, `onMention` and `grip`, and uses the ones it needs.
const SHEETS = {
  task: { Page: TaskPage, label: "Task", refocus: true },
  overview: { Page: ProjectOverview, label: "Overview", refocus: true },
  checks: { Page: ChecksPage, label: "To check", fit: true, refocus: true },
  usage: { Page: UsagePage, label: "Usage", fit: true },
  panels: { Page: PanelsPage, label: "Panels", fit: true },
};

// ^ The conversation of a manager (or a role) as the rows the page holds. A read asks after the
// last entry it has; one read at a time, one more after a change during it. `search` is the one
// record of what the rows were read for: `words` and `around` are what is asked, `read` and
// `readAround` what the shown rows hold. A find's words read the conversation from the oldest entry
// that shows them, not only its newest 400 entries, and those rows stay after the find closes
// (docs/web-client-chat.md "Find"). `around` is the message a tap on a quote opened the chat at
// (docs/web-client-chat.md "Replies"): the rows are a snapshot of that part, no later read adds to
// them, until `back` reads the newest part.
function useManagerLog(id, role) {
  const [chat, setChat] = useState(null);
  const [failure, setFailure] = useState("");
  const [finding, setFinding] = useState(false);
  const [, renew] = useState(0);
  const shown = useRef([]);
  const cut = useRef(false);
  const search = useRef({ words: "", read: "", around: null, readAround: null });
  const aim = (change) => { Object.assign(search.current, change); renew((count) => count + 1); };
  const load = useCoalescedLoad(async (current) => {
    const { words, around: at } = search.current;
    const fresh = words !== search.current.read || at !== search.current.readAround;
    if (at != null && !fresh) return;
    // ^ A queued row is read again until a turn takes it: the read starts after the newest row
    // before the oldest queued one, and the later rows it brings join by their keys.
    const taken = shown.current.filter((entry) => !entry.queued);
    const oldest = Math.min(...shown.current.filter((entry) => entry.queued).map((entry) => entry.id));
    const last = fresh ? null : taken.findLast((entry) => entry.id != null && entry.id < oldest)?.id;
    const next = await api(`api/tasks/${id}/chat`, { query: { ...(last != null && { after: last }), ...(fresh && words && { find: words }), ...(fresh && !words && at != null && { around: at }), ...(role && { role }) } });
    if (!current()) return;
    if (fresh) {
      search.current.read = words;
      search.current.readAround = at;
      cut.current = Boolean(next.skipped);
      if (at != null && !words && !next.rows.some((row) => row.id === at)) {
        // ^ The log no longer has it: the newest part shows.
        aim({ around: null, readAround: null });
        toast("That message is no longer in the conversation.");
      }
      shown.current = next.rows;
      setChat(next);
      setFailure("");
      return;
    }
    // ^ A rotation, a stop or a limit has no id, so every read after the last id returns it
    // again until the next message: a row joins once, by its key, with the newest read's words
    // (a run of stops grows its count).
    const again = new Map(next.rows.map((row) => [row.key, row]));
    const kept = taken.map((row) => again.get(row.key) ?? row);
    const keys = new Set(kept.map((row) => row.key));
    const joined = [...kept, ...next.rows.filter((row) => !keys.has(row.key))];
    if (search.current.read) shown.current = joined;
    else {
      if (next.skipped || joined.length > 400) cut.current = true;
      shown.current = joined.slice(-400);
    }
    setChat({ ...next, rows: shown.current });
    setFailure("");
  }, (error) => setFailure(error.message), [id]);
  useEventLoad("manager", load, (detail) => detail.project == null
    || (detail.project === id && (!role || detail.role === role)), [id]);
  // ^ A tap on a quote whose message is not among the rows (`Quote`, reply.js) reads the chat
  // around it; `back` reads the newest part.
  const jump = (message) => {
    setFinding(false);
    aim({ words: "", around: message });
    load();
  };
  const jumping = useRef(null);
  jumping.current = jump;
  useEffect(() => {
    const on = (event) => jumping.current(event.detail);
    addEventListener("quote-jump", on);
    return () => removeEventListener("quote-jump", on);
  }, []);
  const back = () => {
    aim({ around: null });
    load();
  };
  // ^ The find's words, as typed: the chat reads again for them.
  const findWords = (words) => {
    search.current.words = words.trim();
    if (search.current.words !== search.current.read) load();
  };
  const { around, readAround, words, read } = search.current;
  return { chat, failure, around, aroundRead: around != null && readAround === around, finding, setFinding, cut: cut.current, loading: words !== read, jump, back, findWords };
}

// ^ A read rebuilds every row from the entries it keeps: a row whose fields are the ones it was
// drawn with gives back the same node, which Preact does not render again (docs/web-client-chat.md
// "Long chats"). `context` is what every row reads besides its own fields: a new one starts the
// rows over. Rows that arrive after the first read slide in (`isNew`); the ones of the first read
// and the rows `own` names (the message this device just sent) do not. Returns `show(row)`.
function useRowNodes(rows, { ready, own, context, draw }) {
  const known = useRef(null);
  const fresh = useRef(new Set());
  const drawn = useRef(null);
  if (ready) {
    if (!known.current) known.current = new Set(rows.map((row) => row.key));
    for (const row of rows) {
      if (known.current.has(row.key)) continue;
      known.current.add(row.key);
      if (!own(row)) fresh.current.add(row.key);
    }
  }
  if (drawn.current?.context !== context || drawn.current.rows.size > rows.length + 200) drawn.current = { context, rows: new Map() };
  return (row) => {
    const isNew = fresh.current.has(row.key);
    const kept = drawn.current.rows.get(row.key);
    if (kept && kept.isNew === isNew && sameFields(kept.row, row)) return kept.node;
    const node = draw(row, isNew);
    drawn.current.rows.set(row.key, { row, isNew, node });
    return node;
  };
}

// The project screen with task_view: the manager chat as messages, what is open in the
// reservoir, and with a task or the page in the route its sheet over the chat. With `role`, a
// role's chat (#/p/<project>/r/<role>): the same bubbles without the manager's reservoir,
// approvals and To check items (docs/web-client-tasks.md "Roles").
export function Manager({ project: id, sheet = null, task = null, item = null, role = null }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const overview = store.tasks[id];
  const { chat, failure, around, aroundRead, finding, setFinding, cut, loading, jump, back, findWords } = useManagerLog(id, role);
  // ^ The To check items, each a bubble at the time it was added; read again when the open
  // count changes (a new item, a pass or a fail).
  const [checkItems, setCheckItems] = useState(role ? [] : null);
  const toCheck = overview?.to_check;
  useEffect(() => {
    if (role) return undefined;
    let live = true;
    api(`api/tasks/${id}/checks`).then((value) => { if (live) setCheckItems(value.items); }, () => { if (live) setCheckItems((now) => now ?? []); });
    return () => { live = false; };
  }, [id, toCheck]);
  const ready = Boolean(chat) && checkItems !== null;
  const [sent, setSent] = useState(null);
  const place = role ? `project-${id}-r-${role}` : `project-${id}`;
  const { log, node: logNode, away, unread, since, scrolled, toEnd, reveal } = useFollowEnd(place, ready);
  const field = useRef();
  const [reply, setReply] = useState(null);
  const [gestures, menu] = useReply((line, id) => { setReply({ line, id: id ? Number(id) : null }); field.current?.focus(); });
  // ^ A task's Answer: the composer takes the mention as typed text, so its own input handler
  // keeps the draft (docs/web-client-tasks.md "Project screen").
  const mention = (n, words = "") => {
    const node = field.current;
    const [mentioned, at] = withMention(node.value, n);
    const text = mentioned.slice(0, at) + words + mentioned.slice(at);
    const caret = at + words.length;
    node.focus({ preventScroll: true });
    node.value = text;
    node.setSelectionRange(caret, caret);
    node.dispatchEvent(new Event("input", { bubbles: true }));
  };
  useEffect(() => { if (!overview) loadTasks(id, { missing: true }); }, [id]);
  const newest = () => { toEnd(); back(); };
  // ^ A message of the rows shows in the middle, marked; false when the rows do not hold it.
  const showRow = (messageId) => {
    const node = logNode.current?.querySelector(`[data-id="${messageId}"]`);
    if (!node) return false;
    reveal(node);
    markFound(node);
    return true;
  };
  // ^ The quoted message shows once its part is read.
  useLayoutEffect(() => {
    if (aroundRead) showRow(around);
  }, [around, chat]);
  // Closing the sheet gives the focus back to the composer, in the same commit: after a paint
  // it took the focus from a button tapped in the meantime (a reservoir row's Land).
  const over = SHEETS[sheet];
  const refocus = Boolean(over?.refocus);
  const refocusWas = useRef(refocus);
  useLayoutEffect(() => {
    if (refocusWas.current && !refocus) field.current?.focus({ preventScroll: true });
    refocusWas.current = refocus;
  }, [refocus]);

  // ^ What waits for the owner is the needs and the open questions (docs/web-client-tasks.md "Project screen").
  const asking = role ? [] : (overview?.asks ?? []).map((ask) => ({ kind: "ask", id: ask.id, label: ask.quote }));
  const needing = role ? [] : [...(overview?.needs ?? []), ...asking];
  const [routineList, setRoutineList] = useRoutines(id, !role, chat?.rows?.at(-1)?.key);
  const counts = { running: role ? 0 : (overview?.tasks ?? []).filter(running).length, needs: needing.length, check: role ? 0 : overview?.to_check ?? 0, routines: routineCounts(role ? null : routineList) };
  const [tray, toggleTray, setTray] = useTray(counts);
  const [look, setLook] = useState(null);
  const showNeed = (approval) => { setTray("needs"); setLook(approval); };

  if (!project) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback="#/" /></header>
        <p class="empty">${!store.daemon ? "wsx is not running on the host." : store.loaded ? "This project is not available." : "Loading…"}</p>
      </div>`;
  }
  // ^ A message the held manager has not taken waits in the queue above the composer, as a
  // session's does; it joins the conversation once a turn takes it (wsx-core `waiting_ids`).
  const all = chat?.rows ?? [];
  const entries = all.filter((row) => !row.queued);
  const waiting = all.filter((row) => row.queued);
  const items = role ? [] : overview?.needs ?? [];
  const words = withChips(id);
  const rows = ready ? conversation(entries, overview?.resolutions ?? [], items.filter((item) => item.kind === "approval"), checkItems, chat?.asks ?? []) : [];
  // The sent message shows until the log has it.
  if (sent && !all.some((row) => row.kind === "message" && row.id === sent.id)) {
    const last = rows.at(-1);
    const grouped = last?.kind === "message" && last.author === "owner";
    if (grouped) last.last = false;
    rows.push({ kind: "message", key: "sending", author: "owner", name: "You", at: Date.now(), first: !grouped, last: true, pending: true, ...quoteSplit(sent.text) });
  }
  const firstNew = since == null ? null : rows.find((row) => row.kind === "message" && row.author !== "owner" && !row.pending && row.at > since);
  const [recent, earlier] = useRecent(place, rows, firstNew, { all: finding || around != null, away });
  const find = () => setFinding(!finding);
  // ^ A question's label: its reply is among the rows and shows marked, else the chat reads
  // around it as a quote jump does (`jump`, a log without it says so in a toast).
  // A line inside a closed fold opens it (`wanted`, read by `Fold`).
  const [wanted, setWanted] = useState(null);
  const goToReply = (replyId) => {
    setWanted(replyId);
    if (!showRow(replyId) && !all.some((row) => row.id === replyId)) jump(replyId);
  };
  useLayoutEffect(() => {
    if (wanted != null && (showRow(wanted) || (aroundRead && around === wanted))) setWanted(null);
  }, [wanted, chat, aroundRead]);
  // ^ The typing bubble leaves in the commit that brings the reply: a bubble that stayed to fade
  // out would move the reply down when it went.
  const typing = Boolean(chat?.busy);
  const names = { manager: chat?.manager.name };
  const stopped = chat && !chat.running && entries.at(-1)?.danger;
  // ^ The manager's state only when it matters: typing or stopped (docs/web-client-tasks.md "Project screen").
  const managerWord = typing ? "typing" : stopped ? "stopped" : "";
  const draw = (row, isNew) => row.kind === "message" ? html`<${Bubble} key=${row.key} project=${id} row=${row} words=${words} fresh=${isNew} onNeed=${role ? null : showNeed} />`
    : row.kind === "line" ? html`<${StatusLine} key=${row.key} project=${id} row=${row} fresh=${isNew} />`
    : row.kind === "quiet" ? html`<${QuietReply} key=${row.key} row=${row} fresh=${isNew} />`
    : row.kind === "fold" ? html`<${Fold} key=${row.key} row=${row} render=${show} all=${finding} want=${wanted} />`
    : row.kind === "check" ? html`<${CheckBubble} key=${row.key} project=${id} row=${row} names=${names} fresh=${isNew} onMention=${mention} />`
    : html`<${Note} key=${row.key} row=${row} />`;
  // ^ What the rows read besides their own fields: a new day (a row's time says its day once it is
  // not today's, lib.js `clock`) and the project names (what `<project> #n` reads, `withChips`).
  const context = `${id} ${finding} ${wanted} ${names.manager} ${store.projects.map((other) => `${other.id}:${other.name}`)} ${new Date().toDateString()}`;
  const show = useRowNodes(rows, { ready, own: (row) => (sent && row.id === sent.id) || row.key === "sending", context, draw });
  // ^ A sheet closes as Back does: to the entry under it, else to this chat.
  const leave = () => goBack(`#/p/${id}`);
  // ^ A sheet's Answer goes to this chat, also when another screen is under the sheet.
  const answer = (n, words) => { mention(n, words); leaveFor(`#/p/${id}`); };
  const header = html`
    <header class="bar">
      <${Back} fallback=${role ? `#/p/${id}` : "#/"} />
      <div class="bar-title">
        <span class="bar-label">${role ?? project.name}</span>
        ${!role && !managerWord && chat?.manager.agent && html`<span class="bar-model" title="Model">${agentWords(chat.manager.agent)}</span>`}
        ${managerWord && html`<span class="bar-state" key=${managerWord} data-tone=${typing ? "working" : "danger"}>${typing
          ? html`<span class="typing-dots"><span class="visually-hidden">typing</span><${Dots} /></span>`
          : managerWord}</span>`}
      </div>
      <div class="bar-tools">
        <${FindButton} open=${finding} onClick=${find} />
        ${!role && html`
          <a class="icon-button" href=${`#/p/${id}/page`} aria-label="Overview" title="Overview"><${Icon} name="overview" /></a>
          <${PanelsButton} project=${id} />
          <${ProjectShell} project=${project} /><${More} project=${project} roles=${chat?.roles} />`}
      </div>
    </header>`;
  // ^ The header sits at the bottom, above the composer, when this device chose it (settings.js).
  const low = store.bar === "bottom";
  return html`
    <div class="page manager">
      ${!low && header}
      <div class="chat">
        ${finding && html`<${FindBar} log=${logNode} reveal=${reveal} loading=${loading} onQuery=${findWords} onClose=${() => setFinding(false)} />`}
        ${!role && html`<${SinceLine} project=${id} reply=${rows.findLast((row) => row.kind === "message" && row.author !== "owner")?.at} sent=${sent} />`}
        ${around != null && html`<p class="note around" role="status">Showing an earlier part of the conversation. <button class="link-action" type="button" onClick=${newest}>Back to newest</button></p>`}
        <div class="log-wrap">
          <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label=${`Conversation with ${role ?? "the manager"}`} tabindex="0" ...${gestures}>
            ${cut ? html`<p class="note">Older messages are not shown.</p>` : null}
            ${ready && !rows.length ? html`<p class="empty">${role
              ? `No messages yet. Ask ${role} about its work; it answers from the ledger.`
              : "No messages yet. Ask the manager for a task or a status; it answers from the ledger."}</p>` : null}
            ${earlier}
            ${withNewLine(recent, firstNew, show)}
            ${typing && html`<${Typing} name=${names.manager} />`}
          </div>
          ${away && html`<${JumpButton} unread=${unread} onClick=${around != null ? newest : toEnd} />`}
        </div>
        ${failure && html`<p class="error chat-error" role="alert">${failure}</p>`}
        ${waiting.length > 0 && html`
          <ul class="queue" aria-label=${`Messages waiting for ${role ?? "the manager"}`}>
            ${waiting.map((row) => html`<li key=${row.key} class="queued" data-key=${row.key}><span class="queued-text">${row.text || `${row.attachments?.length ?? 0} files`}</span></li>`)}
          </ul>`}
        ${!role && html`<${Reservoir} project=${id} open=${tray} onClose=${() => toggleTray(tray)} look=${look} onLooked=${() => setLook(null)} items=${needing} tasks=${overview?.tasks ?? []} checks=${overview?.check_items ?? []} routines=${routineList ?? { routines: [] }} onRoutines=${setRoutineList} onMention=${mention} onReply=${(line, id) => { setReply({ line, id }); field.current?.focus(); }} onShow=${goToReply} />`}
        ${low && header}
        ${!role && !can("task_act") && html`<${Pills} counts=${counts} open=${tray} onToggle=${toggleTray} bar />`}
        ${can("task_act") && html`<${ManagerComposer} project=${id} role=${role} busy=${typing} field=${field} reply=${reply} onReply=${setReply}
          pills=${!role && html`<${Pills} counts=${counts} open=${tray} onToggle=${toggleTray} />`}
          onSent=${(message) => { toEnd(); setSent(message); back(); }} />`}
      </div>
      ${menu}
      ${over && html`
        <${Sheet} key=${`${sheet}${task ?? ""}`} label=${task != null ? `${over.label} #${task}` : over.label} fit=${over.fit} onClose=${leave}>
          ${(grip) => html`<${over.Page} project=${id} task=${task} item=${item} onClose=${leave} onMention=${answer} grip=${grip} />`}
        </${Sheet}>`}
    </div>`;
}

// A rotation or a stop of the manager, said once.
const Note = ({ row }) => html`<p class="note" data-tone=${row.danger ? "danger" : null}>${row.text}</p>`;
