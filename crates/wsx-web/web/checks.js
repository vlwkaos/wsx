// The project's To check list: what only a person can check of the landed tasks, passed or
// failed at the owner's own time (docs/tasks.md "To check", docs/web-client-tasks.md "To check").
import { api } from "./api.js";
import { html, Icon, Loading, useEffect, useRef, useState } from "./lib.js";
import { can, useStore } from "./store.js";
import { loadTasks, TaskChip } from "./tasks.js";

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
  return html`<p class="check-pass">Look at ${link
    ? html`<a href=${where} target="_blank" rel="noopener">${where}</a>`
    : where}</p>`;
}

// One item at the full width: the task chip starting what the task did, the check, where to
// look and its capture, then when it passes with Try, Fail and Pass as icons at its right end;
// a long pass line takes the whole row and the icons wrap under it, so no words wrap beside them.
// A fail needs a note, which goes to the task that fixes it. `revision` is the task's current one: an item of an older revision names its own.
export function CheckRow({ project, item, revision, onChanged }) {
  const [failing, setFailing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const note = useRef();
  useEffect(() => { if (failing) note.current.focus(); }, [failing]);
  const decide = async (decision, body = {}) => {
    setBusy(true);
    setError("");
    try {
      await api(`api/tasks/${project}/checks/${item.id}/${decision}`, { method: "POST", json: body });
      setFailing(false);
      loadTasks(project);
      onChanged?.();
    } catch (failure) {
      setError(failure.message);
    }
    setBusy(false);
  };
  const fail = (event) => {
    event.preventDefault();
    const text = note.current.value.trim();
    if (text) decide("fail", { note: text });
  };
  const acting = can("task_act") && item.state === "open";
  const older = revision != null && item.revision !== revision ? ` (r${item.revision})` : "";
  const capture = `api/tasks/${project}/checks/${item.id}/capture`;
  const did = item.did ?? item.title;
  const chip = html`<${TaskChip} project=${project} id=${item.task} />`;
  return html`
    <li class="check-item" data-state=${item.state}>
      <div class="check-line">
        ${did && html`<p class="check-task">${chip} ${did}</p>`}
        <p class="check-what">${!did && html`${chip} `}${item.check}${older}</p>
        <${Place} where=${item.where} />
        ${item.capture && html`
          <a class="check-capture" href=${capture} target="_blank" rel="noopener" aria-label=${`Capture of: ${item.check}`}>
            <img src=${capture} alt="" />
          </a>`}
        <div class="check-end">
          <p class="check-pass">Passes if ${item.pass_if}</p>
          ${acting && !failing && html`
            <span class="check-acts">
              <${TryButton} project=${project} task=${item.task} icon />
              <button class="icon-button check-act" type="button" data-tone="danger" disabled=${busy} title="Fail"
                aria-label=${`Fail: ${item.check}`} onClick=${() => setFailing(true)}><${Icon} name="fail" /></button>
              <button class="icon-button check-act" type="button" data-tone="accent" disabled=${busy} title="Pass"
                aria-label=${`Pass: ${item.check}`} onClick=${() => decide("pass")}><${Icon} name="pass" /></button>
            </span>`}
        </div>
        ${item.state === "failed" && html`<p class="check-pass">Failed: ${item.note}${item.fix_task != null ? html` · <${TaskChip} project=${project} id=${item.fix_task} /> fixes it` : null}</p>`}
        ${item.state === "passed" && html`<p class="check-pass">Passed</p>`}
      </div>
      ${failing && html`
        <form class="need-form" onSubmit=${fail}>
          <input class="input" ref=${note} type="text" autocomplete="off" disabled=${busy}
            aria-label=${`What is wrong with: ${item.check}`} placeholder="What is wrong (a task fixes it)" />
          <button class="button" type="button" disabled=${busy} onClick=${() => setFailing(false)}>Cancel</button>
          <button class="button" type="submit" disabled=${busy}>Fail</button>
        </form>`}
      ${error && html`<p class="error need-error" role="alert">${error}</p>`}
    </li>`;
}

// Every open item of the project in its sheet, oldest first, with All passed in the top bar.
export function ChecksPage({ project, onClose, grip }) {
  const store = useStore();
  const [items, setItems] = useState(null);
  const [failure, setFailure] = useState("");
  const [busy, setBusy] = useState(false);
  const live = useRef(true);
  const read = () => api(`api/tasks/${project}/checks`).then(
    (value) => { if (live.current) { setItems(value.open); setFailure(""); } },
    (error) => { if (live.current) setFailure(error.message); });
  useEffect(() => {
    live.current = true;
    return () => { live.current = false; };
  }, []);
  // ^ The overview counts the open items; a change anywhere reads the list again.
  const count = store.tasks[project]?.to_check;
  useEffect(() => { read(); }, [project, count]);
  // ^ The sheet opens once the first read and its captures are in (tasks.js `Sheet`).
  const page = useRef();
  const loaded = items !== null || Boolean(failure);
  useEffect(() => {
    if (!loaded) return;
    const images = [...page.current.querySelectorAll(".check-capture img")];
    Promise.all(images.map((image) => image.decode().catch(() => {}))).then(() => grip.ready());
  }, [loaded]);
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
    <div class="page task-page" ref=${page}>
      <header class="bar" onPointerDown=${grip?.down} onPointerMove=${grip?.move} onPointerUp=${grip?.up} onPointerCancel=${grip?.up}>
        <button class="icon-button" type="button" aria-label="Back to the chat" onClick=${onClose}><${Icon} name="back" /></button>
        <div class="bar-title">
          <span class="bar-label">To check</span>
          ${items && html`<span class="bar-state">${items.length ? `${items.length} open` : "nothing open"}</span>`}
        </div>
        ${items?.length > 0 && can("task_act") && html`
          <div class="bar-tools">
            <button class="button check-all" type="button" disabled=${busy} onClick=${passAll}>${busy ? "Passing…" : "All passed"}</button>
          </div>`}
      </header>
      <div class="scroll">
        ${failure && html`<p class="error" role="alert">${failure}</p>`}
        ${!items ? (!failure && html`<${Loading} />`)
          : !items.length ? html`<p class="empty">Nothing to check.</p>`
          : html`
            <ul class="check-list" aria-label="Items to check">
              ${items.map((item) => html`<${CheckRow} key=${item.id} project=${project} item=${item} onChanged=${read} />`)}
            </ul>`}
      </div>
    </div>`;
}
