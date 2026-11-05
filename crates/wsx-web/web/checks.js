// The project's To check list: what only a person can check of the landed tasks, passed or
// failed at the owner's own time (docs/tasks.md "To check", docs/web-client-tasks.md "To check").
import { api } from "./api.js";
import { html, Icon, Loading, useCoalescedLoad, useEffect, useState } from "./lib.js";
import { can, useStore } from "./store.js";
import { loadTasks, TaskChip } from "./tasks.js";
import { SheetBar } from "./ui.js";

// Try: start the task's preview, or find the running one, then a link to open it. A link
// the person opens keeps popup blockers out of the way. Only for projects with
// `tasks.preview` (docs/tasks.md "Preview").
export function TryButton({ project, task, icon = false }) {
  const store = useStore();
  const [state, setState] = useState({ busy: false, url: null, error: "" });
  if (!store.tasks[project]?.preview || !can("task_act")) return null;
  const start = async () => {
    setState({ busy: true, url: null, error: "" });
    try {
      const answer = await api(`api/tasks/${project}/t/${task}/preview`, { method: "POST", json: {} });
      setState({ busy: false, url: answer.url, error: "" });
    } catch (failure) {
      setState({ busy: false, url: null, error: failure.message });
    }
  };
  const label = icon ? "icon-button check-act" : "button need-action";
  return html`
    ${state.url
      ? html`<a class=${label} href=${state.url} target="_blank" rel="noopener" title="Open the preview"
          aria-label=${`Open the preview of #${task}`}><${Icon} name="open" />${!icon && html`<span class="button-words">Open</span>`}</a>`
      : html`<button class=${label} type="button" disabled=${state.busy} onClick=${start} title="Try it on a preview"
          aria-label=${`Try #${task} on a preview`}><${Icon} name="play" />${!icon && html`<span class="button-words">${state.busy ? "Starting…" : "Try"}</span>`}</button>`}
    ${state.error && html`<span class="error" role="alert">${state.error}</span>`}`;
}

// Where a person looks: a link when it is a URL or a page path, else the words.
function Place({ where }) {
  if (!where) return null;
  const link = /^(https?:\/\/|\/|#)/.test(where);
  return html`<dt>Look at</dt><dd>${link
    ? html`<a href=${where} target="_blank" rel="noopener">${where}</a>`
    : where}</dd>`;
}

// One item: the task chip with what the task did as its title, then the check, where to look,
// its capture and when it passes as labeled lines. Try, Fail, Skip and Pass are icons in one place on
// every item: the bottom right on a phone, a column right of the words on a wide screen.
// Fail puts the item into the manager's composer (`onMention`, as the Fail of tasks.js `NeedRow`
// does); the item stays open and the manager starts the fix. Pass and Skip show their result at once.
// `revision` is the task's current one: an item of an older revision names its own.
// `tag`: an `li` in the list, a `div` in its chat bubble (bubbles.js `CheckBubble`).
export function CheckRow({ project, item: listed, revision, onChanged, onMention, tag = "li" }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  // ^ The decision answers with the item's new state; a chat bubble has no list to read again.
  const [decided, setDecided] = useState(null);
  const item = decided?.id === listed.id && listed.state === "open" ? decided : listed;
  const decide = async (decision) => {
    setBusy(true);
    setError("");
    try {
      const answer = await api(`api/tasks/${project}/checks/${item.id}/${decision}`, { method: "POST", json: {} });
      setDecided({ ...item, ...answer.changed?.find((changed) => changed.id === item.id), state: decision === "pass" ? "passed" : "skipped" });
      loadTasks(project);
      onChanged?.();
    } catch (failure) {
      setError(failure.message);
    }
    setBusy(false);
  };
  // ^ The dialog closes in the tap, as the Answer of task.js does, so the composer can take the focus.
  const fail = (event) => {
    event.currentTarget.closest("dialog")?.close();
    onMention(item.task, `To check ${item.id} failed: `);
  };
  const acting = can("task_act") && item.state === "open";
  const older = revision != null && item.revision !== revision ? ` (r${item.revision})` : "";
  const capture = `api/tasks/${project}/checks/${item.id}/capture`;
  // ^ A video capture shows its poster (or a blank frame) with a play mark; the viewer plays it.
  const video = item.capture?.endsWith(".mp4");
  const poster = video && item.poster ? `api/tasks/${project}/checks/${item.id}/poster` : undefined;
  const did = item.did ?? item.title;
  return html`
    <${tag} class="check-item" data-state=${item.state} data-gallery>
      <div class="check-line">
        <div class="check-words">
          <p class="check-task"><${TaskChip} project=${project} id=${item.task} /> ${did}</p>
          <dl class="facts check-facts">
            <dt>Check</dt><dd class="check-what">${item.check}${older}</dd>
            <${Place} where=${item.where} />
            <dt>Passes if</dt><dd class="check-pass">${item.pass_if}</dd>
          </dl>
          ${item.capture_removed && html`<p class="check-state">Capture removed</p>`}
          ${item.capture && !item.capture_removed && html`
            <a class="check-capture" href=${capture} data-view data-download=${`${capture}?download=1`} data-video=${video || undefined} data-poster=${poster}
              aria-label=${`${video ? "Video" : "Capture"} of: ${item.check}`}>
              ${video && !poster ? html`<span class="check-frame"></span>` : html`<img src=${poster ?? capture} alt="" style=${item.capture_size && { "--w": item.capture_size[0], "--h": item.capture_size[1], "--width": `${item.capture_size[0]}px` }} />`}
              ${video && html`<span class="check-play"><${Icon} name="play" /></span>`}
            </a>`}
          ${item.state === "failed" && html`<p class="check-state">Failed: ${item.note}${item.fix_task != null ? html` · <${TaskChip} project=${project} id=${item.fix_task} /> fixes it` : null}</p>`}
          ${item.state === "passed" && html`<p class="check-state">Passed</p>`}
          ${item.state === "skipped" && html`<p class="check-state">Skipped</p>`}
        </div>
        ${acting && html`
          <span class="check-acts">
            <${TryButton} project=${project} task=${item.task} icon />
            ${onMention && html`<button class="icon-button check-act" type="button" data-tone="danger" disabled=${busy}
              title="Fail: tell the manager" aria-label=${`Fail: tell the manager what is wrong with: ${item.check}`} onClick=${fail}><${Icon} name="fail" /></button>`}
            <button class="icon-button check-act" type="button" disabled=${busy} title="Not checking: close it without a verdict"
              aria-label=${`Skip, not checking: close it without a verdict: ${item.check}`} onClick=${() => decide("skip")}><${Icon} name="skip" /></button>
            <button class="icon-button check-act" type="button" data-tone="accent" disabled=${busy} title="Works: close this check"
              aria-label=${`Pass, works: close this check: ${item.check}`} onClick=${() => decide("pass")}><${Icon} name="pass" /></button>
          </span>`}
      </div>
      ${error && html`<p class="error need-error" role="alert">${error}</p>`}
    </${tag}>`;
}

// Every open item of the project in its sheet, oldest first, with All passed in the top bar.
export function ChecksPage({ project, onClose, onMention, grip }) {
  const store = useStore();
  const [items, setItems] = useState(null);
  const [failure, setFailure] = useState("");
  const [busy, setBusy] = useState(false);
  const read = useCoalescedLoad(async (current) => {
    const value = await api(`api/tasks/${project}/checks`);
    if (!current()) return;
    setItems(value.open);
    setFailure("");
  }, (error) => setFailure(error.message), [project]);
  // ^ The overview counts the open items; a change anywhere reads the list again.
  const count = store.tasks[project]?.to_check;
  useEffect(() => { read(); }, [project, count]);
  // ^ The sheet opens once the first read is in (tasks.js `Sheet`); each capture's box has its
  // size by then, so nothing that comes later moves the sheet.
  const loaded = items !== null || Boolean(failure);
  useEffect(() => { if (loaded) grip.ready(); }, [loaded]);
  const passAll = async () => {
    setBusy(true);
    setFailure("");
    try {
      await api(`api/tasks/${project}/checks/all/pass`, { method: "POST", json: {} });
      loadTasks(project);
      await read();
    } catch (error) {
      setFailure(error.message);
    }
    setBusy(false);
  };
  return html`
    <div class="page task-page">
      <${SheetBar} grip=${grip} onClose=${onClose} tools=${items?.length > 0 && can("task_act") && html`
        <div class="bar-tools">
          <button class="button check-all" type="button" disabled=${busy} onClick=${passAll}>${busy ? "Passing…" : "All passed"}</button>
        </div>`}>
        <span class="bar-label">To check</span>
        ${items && html`<span class="bar-state">${items.length ? `${items.length} open` : "nothing open"}</span>`}
      </${SheetBar}>
      <div class="scroll">
        ${failure && html`<p class="error" role="alert">${failure}</p>`}
        ${!items ? (!failure && html`<${Loading} />`)
          : !items.length ? html`<p class="empty">Nothing to check.</p>`
          : html`
            <ul class="check-list" aria-label="Items to check">
              ${items.map((item) => html`<${CheckRow} key=${item.id} project=${project} item=${item} onChanged=${read} onMention=${onMention} />`)}
            </ul>`}
      </div>
    </div>`;
}
