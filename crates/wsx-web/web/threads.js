// A chat's threads in its window (docs/design/chat-system.md "Using threads"): the strip above the
// composer, the line in the chat for a thread that waits for the owner, a thread's ⋯ menu and the
// New thread form. The order and the counts are the server's (`thread::list` over `needs`): this
// file only draws them.
import { api } from "./api.js";
import { html, Icon, useEffect, useRef, useState } from "./lib.js";
import { chatHash, leaveFor, replaceRoute } from "./store.js";
import { confirmDialog, MoreMenu, toast } from "./ui.js";
import { TRAYS } from "./trays.js";

// A thread's window is `#/c/<chat>/t/<slug>`; the chat's own rows are `#/c/<chat>`.
export const threadHash = (chat, slug = null) => `${chatHash(chat)}${slug ? `/t/${encodeURIComponent(slug)}` : ""}`;

// ^ What a thing says of its thread is `<chat>/<slug>` (a task, an item) or the bare slug (a
// question); one belongs to the open thread `thread` when its slug is among `thread.slugs`
// (the thread and the ones merged into it).
export const inThread = (thread, thing) => thread.slugs.includes(String(thing.thread ?? "").split("/").pop());

const YOU = TRAYS[0];

// The chat first, then the threads as the server lists them; `+` at the end opens New thread.
export function ThreadStrip({ chat, threads, current, onNew }) {
  const chip = (slug, name, counts = null) => {
    const waiting = counts ? YOU.count(counts) : 0;
    return html`
      <a class="thread-chip" key=${slug ?? ""} href=${threadHash(chat, slug)} aria-current=${current === slug} onClick=${(event) => replaceRoute(event, threadHash(chat, slug))}>
        <span class="thread-name">${name}</span>${waiting > 0 && html`<span class="thread-ask" role="img" aria-label=${`${waiting} for you`}>${waiting}</span>`}
      </a>`;
  };
  return html`
    <nav class="threads" aria-label="Threads">
      ${chip(null, "Chat")}
      ${threads.map((found) => chip(found.slug, found.title, found.counts))}
      ${onNew && html`<button class="thread-new" type="button" aria-label="New thread" onClick=${onNew}><${Icon} name="plus" /></button>`}
    </nav>`;
}

// One line in the chat for each thread that waits for the owner.
export const ThreadWaits = ({ chat, threads }) => threads.filter((found) => found.waits).map((found) => html`
  <p class="note thread-waits" key=${found.slug}><b>${found.title}</b> waits for you · ${found.waits} · <a href=${threadHash(chat, found.slug)}>Open</a></p>`);

// ^ A thread's ⋯ menu: Merge into another open thread, and Close. The core refuses a close while a
// question of the thread waits for the owner, and says why (409); the words show as they are.
export function ThreadMore({ project, chat, thread, others }) {
  const [merging, setMerging] = useState(false);
  const act = (action, json) => api(`api/tasks/${project}/threads/${action}`, { method: "POST", json: { chat, thread: thread.slug, title: "", into: "", ...json } });
  const close = async () => {
    const sure = await confirmDialog({ title: `Close ${thread.title}?`, text: "Its messages and tasks stay and read as the chat's own.", confirm: "Close thread" });
    if (!sure) return;
    try {
      await act("close");
      leaveFor(threadHash(chat));
    } catch (failure) {
      toast(failure.message, { failure: true });
    }
  };
  return html`
    <${MoreMenu}>
      ${others.length > 0 && html`<button type="button" role="menuitem" onClick=${() => setMerging(true)}>Merge into…</button>`}
      <button type="button" role="menuitem" onClick=${close}>Close thread</button>
    </${MoreMenu}>
    ${merging && html`<${MergeThread} thread=${thread} others=${others} onSend=${(into) => act("merge", { into })} onDone=${() => leaveFor(threadHash(chat))} onClose=${() => setMerging(false)} />`}`;
}

// A form in a sheet: its fields after the title, `submit` the call that makes the change; its refusal
// shows under the field, a success closes the sheet and runs `onDone`.
function FormSheet({ id, title, submitLabel, submit, onDone, onClose, children }) {
  const dialog = useRef();
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => { dialog.current.showModal(); dialog.current.querySelector("input, select")?.focus(); }, []);
  const send = async (event) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      const done = await submit(new FormData(event.target));
      dialog.current.close();
      onDone(done);
    } catch (failure) {
      setError(failure.message);
      setBusy(false);
    }
  };
  return html`
    <dialog class="sheet" ref=${dialog} aria-labelledby=${id} onClose=${onClose}>
      <form onSubmit=${send} noValidate>
        <h2 class="sheet-title" id=${id}>${title}</h2>
        ${children}
        <p class="error" role="alert">${error}</p>
        <div class="actions sheet-actions">
          <button class="button" type="button" onClick=${() => dialog.current.close()}>Cancel</button>
          <button class="button button-primary" type="submit" disabled=${busy}>${submitLabel}</button>
        </div>
      </form>
    </dialog>`;
}

function MergeThread({ thread, others, onSend, onDone, onClose }) {
  return html`
    <${FormSheet} id="merge-thread-title" title=${`Merge ${thread.title}`} submitLabel="Merge" submit=${(form) => onSend(form.get("into"))} onDone=${onDone} onClose=${onClose}>
      <label class="field">Into
        <select class="input" name="into">${others.map((found) => html`<option key=${found.slug} value=${found.slug}>${found.title}</option>`)}</select>
        <span class="field-note">Its messages and tasks join that thread; this one closes.</span>
      </label>
    </${FormSheet}>`;
}

// The New thread form: a title; the thread opens and its window shows.
export function NewThread({ project, chat, onClose }) {
  const submit = (form) => api(`api/tasks/${project}/threads/open`, { method: "POST", json: { chat, thread: "", title: form.get("title"), into: "" } });
  return html`
    <${FormSheet} id="new-thread-title" title="New thread" submitLabel="Open" submit=${submit} onDone=${(done) => { location.hash = threadHash(chat, done.thread); }} onClose=${onClose}>
      <label class="field">Title
        <input class="input" type="text" name="title" autocomplete="off" enterkeyhint="done" />
        <span class="field-note">Messages you write in the thread, and the tasks it starts, stay together.</span>
      </label>
    </${FormSheet}>`;
}
