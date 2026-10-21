// Chat: the agent's own saved conversation as a typed timeline, what the agent does
// now, the messages that wait for it, and the composer (docs/web-client.md "Chat").
import { api, resolve } from "./api.js";
import { bytes, elapsed, html, Icon, useCallback, useEffect, useFollowEnd, useLayoutEffect, useNow, useRef, useState } from "./lib.js";
import { can, events, shows, stateOf, state, touch } from "./store.js";
import { Markdown } from "./markdown.js";

// Unsent text, attachments, files that still upload and the message that is not in the
// saved conversation yet, per session. Kept outside the view so a route change does not
// lose them. Text, uploaded attachments and a message that was not delivered are also kept
// on this device, so a reload or a phone that drops the page loses nothing without a word.
const drafts = new Map();
const DRAFTS_KEY = "wsx-drafts";
function stored() {
  try { return JSON.parse(localStorage.getItem(DRAFTS_KEY)) ?? {}; } catch { return {}; }
}
const draftFor = (id) => {
  if (!drafts.has(id)) {
    const saved = stored()[id];
    drafts.set(id, { text: saved?.text ?? "", attachments: saved?.attachments ?? [], uploads: [] });
  }
  return drafts.get(id);
};
function keepDraft(id) {
  const all = stored();
  const { text, attachments } = drafts.get(id);
  if (text || attachments.length) all[id] = { text, attachments };
  else delete all[id];
  try { localStorage.setItem(DRAFTS_KEY, JSON.stringify(all)); } catch { /* a private window keeps the draft in memory only */ }
}

// ^ Owner's rule: a photo's long edge is at most 2000 px, as JPEG, and a PNG (a screenshot)
// stays PNG when that is smaller. Anything that fails, or is not smaller, keeps the original.
const LONG_EDGE = 2000;
async function shrink(file) {
  if (!/^image\/(png|jpeg|webp|heic|heif)$/.test(file.type)) return file;
  try {
    const bitmap = await createImageBitmap(file);
    const scale = Math.min(1, LONG_EDGE / Math.max(bitmap.width, bitmap.height));
    const encode = (type, background) => {
      const canvas = document.createElement("canvas");
      canvas.width = Math.round(bitmap.width * scale);
      canvas.height = Math.round(bitmap.height * scale);
      const context = canvas.getContext("2d");
      // JPEG has no transparency: what was transparent becomes white, not black.
      if (background) { context.fillStyle = background; context.fillRect(0, 0, canvas.width, canvas.height); }
      context.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
      return new Promise((done) => canvas.toBlob(done, type, 0.85));
    };
    const jpeg = await encode("image/jpeg", "#fff");
    const png = file.type === "image/png" ? await encode("image/png") : null;
    bitmap.close();
    const best = png && (!jpeg || png.size <= jpeg.size) ? png : jpeg;
    if (!best || best.size >= file.size) return file;
    const stem = file.name.replace(/\.[^.]*$/, "") || "image";
    return new File([best], `${stem}.${best.type === "image/png" ? "png" : "jpg"}`, { type: best.type });
  } catch {
    return file;
  }
}

// Send one message. The server keeps it in the session's queue until the saved
// conversation shows it (docs/web-client.md "Queue and Stop").
const post = (id, text, attachments) => api(`api/chat/${id}/send`, {
  method: "POST", json: { text, attachments: attachments.map((item) => item.hash) },
});

// Copy a message's text, so a message the agent never took is not lost (docs/web-client.md "Queue and Stop").
function CopyText({ text, label }) {
  const [said, setSaid] = useState("");
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setSaid("Copied");
    } catch {
      setSaid("Copy failed");
    }
    setTimeout(() => setSaid(""), 2000);
  };
  return html`<button class="link-action" type="button" aria-label=${label} aria-live="polite" onClick=${copy}>${said || "Copy"}</button>`;
}

// wsxd's reason without its code: "editor_unavailable: Claude input editor is not visibly
// idle" reads "Claude input editor is not visibly idle".
const inWords = (reason) => reason.replace(/^[a-z_]+: /, "");

// The most items the server sends (crates/wsx-web/src/chat.rs TIMELINE_ITEMS).
const TIMELINE_ITEMS = 400;

const EMPTY_WORDS = {
  no_agent: "No agent runs in this session.",
  identity_unavailable: "The agent has not reported its conversation yet.",
  unsupported_provider: "This agent type has no readable saved conversation.",
  history_not_found: "The saved conversation file was not found.",
  history_unreadable: "No messages yet.",
  no_readable_messages: "No messages yet.",
};
// What the latest exchange says under the last message of the conversation. A failed
// delivery is not here: that message never reached the conversation, so the waiting message
// says it.
const DELIVERY_WORDS = {
  cancelled: "Stopped.",
  cancel_requested: "Stopping…",
  expired: "The agent did not finish in time.",
  interrupted: "The turn was interrupted.",
  target_exited: "The agent's program exited.",
  target_replaced: "The agent's program was replaced.",
};

export const Attachments = ({ files }) => files?.length ? html`
  <ul class="attachments">
    ${files.map((file) => html`
      <li key=${file.hash}>
        ${file.image
          ? html`<a href=${resolve(file.url)}><img src=${resolve(file.url)} alt=${file.name} loading="lazy" /></a>`
          : html`<a href=${resolve(file.url)} download=${file.name}>${file.name} (${bytes(file.size)})</a>`}
      </li>`)}
  </ul>` : null;

// Upload picked files into `files` ({ attachments, uploads }). Every file shows at once as
// uploading, and Send waits until all are in. `changed` re-renders; `fail` says why one did
// not upload. The session chat and the manager chat share it.
export async function uploadFiles(picked, files, changed, fail) {
  files.uploads.push(...picked);
  changed();
  for (const original of picked) {
    const item = await shrink(original);
    if (item.size > state.site.max_file_bytes) {
      fail(`${item.name} is larger than ${bytes(state.site.max_file_bytes)}.`);
    } else {
      try {
        files.attachments.push(await api("api/media", { method: "PUT", query: { name: item.name }, body: item }));
      } catch (failure) {
        fail(`${item.name}: ${failure.message}`);
      }
    }
    files.uploads = files.uploads.filter((other) => other !== original);
    changed();
  }
}

// The files a message will carry, each with Remove, and the ones still uploading.
export const FileChips = ({ files, onRemove }) => files.attachments.length || files.uploads.length ? html`
  <ul class="chips" aria-label="Files to send">
    ${files.attachments.map((item) => html`
      <li key=${item.hash}><span>${item.name} (${bytes(item.size)})</span>
        <button class="link-action" type="button" aria-label=${`Remove ${item.name}`} onClick=${() => onRemove(item)}>Remove</button>
      </li>`)}
    ${files.uploads.map((item) => html`<li data-uploading><span>${item.name}</span><span class="note" role="status">Uploading…</span></li>`)}
  </ul>` : null;

// The attach button and its hidden file picker, when the site allows uploads.
export function AttachButton({ onFiles }) {
  const picker = useRef();
  if (!can("media_upload")) return null;
  return html`
    <button class="icon-button" type="button" aria-label="Attach files" onClick=${() => picker.current.click()}><${Icon} name="attach" /></button>
    <input type="file" multiple hidden tabIndex="-1" ref=${picker}
      onChange=${(event) => { const picked = [...event.target.files]; event.target.value = ""; onFiles(picked); }} />`;
}

const stepWord = (item) => item.kind === "thinking" ? "Thinking" : item.tool.name;
const Step = ({ item }) => html`
  <li class="step">
    <span class="step-title">${item.kind === "thinking" ? "Thinking" : item.tool.title}</span>
    ${item.tool?.status === "error" && html`<span class="step-state" data-tone="danger">failed</span>`}
  </li>`;

// Consecutive tool and thinking items between two messages are one line. The run that
// is still going shows its last three finished items: the step that runs now is in
// the working row and nowhere else.
function Steps({ items, live }) {
  if (live) {
    const finished = items.filter((item) => item.tool?.status !== "running");
    // With no tool running, a last thinking item is what the agent does now.
    items = finished.length === items.length && items.at(-1).kind === "thinking" ? items.slice(0, -1) : finished;
    if (!items.length) return null;
  }
  const names = [...new Set(items.map(stepWord))];
  const summary = `${items.length} ${items.length === 1 ? "step" : "steps"}: ${names.join(", ")}`;
  if (live) {
    const earlier = items.length - 3;
    return html`
      <div class="steps" data-live>
        ${earlier > 0 && html`<p class="steps-summary">${earlier} earlier ${earlier === 1 ? "step" : "steps"}</p>`}
        <ul class="step-list">${items.slice(-3).map((item) => html`<${Step} key=${item.id} item=${item} />`)}</ul>
      </div>`;
  }
  return html`
    <details class="steps">
      <summary class="steps-summary"><${Icon} name="chevron" />${summary}</summary>
      <ul class="step-list">${items.map((item) => html`<${Step} key=${item.id} item=${item} />`)}</ul>
    </details>`;
}

// A compaction summary, where the conversation was compacted. Its start stays open, so
// a session continued after a compaction still says what it is about.
function Summary({ id, text }) {
  const body = useRef();
  const [open, setOpen] = useState(false);
  const [long, setLong] = useState(false);
  useLayoutEffect(() => {
    if (!open) setLong(body.current.scrollHeight > body.current.clientHeight + 1);
  }, [text, open]);
  return html`
    <section class="summary" aria-labelledby=${`${id}-label`}>
      <p class="summary-label" id=${`${id}-label`}>Summary of the earlier conversation</p>
      <div class="summary-body" id=${`${id}-body`} ref=${body} data-clamped=${!open && long ? "" : undefined} data-open=${open ? "" : undefined}>
        <${Markdown} text=${text} />
      </div>
      ${(long || open) && html`
        <button class="link-action" type="button" aria-expanded=${open} aria-controls=${`${id}-body`} onClick=${() => setOpen(!open)}>
          ${open ? "Show less" : "Show full summary"}
        </button>`}
    </section>`;
}

// Messages as they are, with each run of steps between them as one block.
function blocks(items) {
  const result = [];
  for (const item of items) {
    const last = result.at(-1);
    if (item.kind === "tool" || item.kind === "thinking") {
      if (last?.steps) last.steps.push(item);
      else result.push({ id: `steps-${item.id}`, steps: [item] });
    } else if (item.text || item.attachments?.length) {
      result.push(item);
    }
  }
  return result;
}

// The agent's commands for the `/` list, read once per session while the page is open.
const commandLists = new Map();
function loadCommands(id) {
  if (!commandLists.has(id)) {
    commandLists.set(id, api(`api/chat/${id}/commands`).catch((failure) => {
      commandLists.delete(id);
      throw failure;
    }));
  }
  return commandLists.get(id);
}

// While the field holds only `/` and a partial name: the commands that match it, names
// that start with it first. `null` when no list applies.
function matching(listing, text) {
  const typed = /^\/(\S*)$/.exec(text);
  if (!listing || !typed) return null;
  const query = typed[1].toLowerCase();
  const starts = listing.commands.filter((command) => command.name.toLowerCase().startsWith(query));
  const within = listing.commands.filter((command) => !starts.includes(command) && command.name.toLowerCase().includes(query));
  return [...starts, ...within];
}

const SOURCE_WORDS = { builtin: "built-in", project: "project", user: "yours", plugin: "plugin" };

// The `/` list above the composer row: a listbox that the field controls. A pointer
// press does not move focus, so the on-screen keyboard stays open.
// ^ iOS shows its form bar (previous, next, Done) above the keyboard; the arrows go to other
// form fields on the page. While the composer has the focus, no other field takes part.
// The bar itself belongs to WebKit and stays (https://bugs.webkit.org/show_bug.cgi?id=251787).
const OTHER_FIELDS = "input, select, textarea:not(.composer-field)";
export function isolate() {
  for (const node of document.querySelectorAll(OTHER_FIELDS)) {
    if (node.hasAttribute("data-tab-index")) continue;
    node.dataset.tabIndex = node.getAttribute("tabindex") ?? "";
    node.tabIndex = -1;
  }
}
export function release() {
  for (const node of document.querySelectorAll("[data-tab-index]")) {
    if (node.dataset.tabIndex === "") node.removeAttribute("tabindex");
    else node.setAttribute("tabindex", node.dataset.tabIndex);
    delete node.dataset.tabIndex;
  }
}

// The command a field starts with, as `/name`, when the agent lists it.
function tokenOf(listing, text) {
  const name = /^\/([^\s/]+)(?=\s|$)/.exec(text)?.[1];
  return name && listing?.commands.some((command) => command.name === name) ? `/${name}` : null;
}

// A list of suggestions above a composer: the `/` commands and the `#` tasks. Each option
// is `{ key, name, description, source }`; `notes` are muted lines at the end of its box.
export const Suggestions = ({ label, options, active, onPick, onHover, notes }) => html`
  <div class="slash">
    <ul class="slash-list" id="slash-list" role="listbox" aria-label=${label}>
      ${options.map((option, index) => html`
        <li key=${option.key} id=${`slash-${index}`} class="slash-option" role="option" aria-selected=${index === active}
          onPointerDown=${(event) => event.preventDefault()} onPointerEnter=${() => onHover(index)} onClick=${() => onPick(option)}>
          <span class="slash-name">${option.name}</span>
          <span class="slash-description">${option.description}</span>
          <span class="slash-source" data-tone=${option.tone}>${option.source}</span>
        </li>`)}
    </ul>
    ${notes.filter(Boolean).map((note) => html`<p class="slash-note">${note}</p>`)}
  </div>`;

const CommandList = ({ listing, matches, active, onPick, onHover }) => html`
  <${Suggestions} label="Commands" active=${active} onHover=${onHover}
    options=${matches.map((command) => ({ key: command.name, name: `/${command.name}`, description: command.description,
      source: SOURCE_WORDS[command.source], command }))}
    onPick=${(option) => onPick(option.command)}
    notes=${[!matches.length && "No command starts with that.",
      !listing.listed && `Only the built-in commands of ${listing.provider || "this agent"} are listed.`,
      !listing.runs && "Upgrade wsx on the host to run commands from here."]} />`;

// A composer field grows with its text, up to six lines.
export function grow(node) {
  node.style.height = "auto";
  const line = parseFloat(getComputedStyle(node).lineHeight);
  node.style.height = `${Math.min(node.scrollHeight, line * 6 + (node.offsetHeight - node.clientHeight))}px`;
}

function Composer({ session, working, onSent }) {
  const draft = draftFor(session.id);
  const [, renew] = useState(0);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const field = useRef();
  const mirror = useRef();
  const ready = Boolean(session.agent?.attached && session.agent.prompt) && !session.exited;
  const [listing, setListing] = useState(null);
  const [active, setActive] = useState(0);
  // Escape closes the list until the text changes.
  const [closed, setClosed] = useState(null);
  const matches = closed === draft.text ? null : matching(listing, draft.text);
  // ^ A listed command at the start of the field shows as one token in the accent colour
  // (an overlay behind the transparent field); the text the agent gets is unchanged.
  const token = tokenOf(listing, draft.text);
  const listOpen = Boolean(matches) && ready;
  useEffect(() => {
    if (!ready || !can("chat_send") || !draft.text.startsWith("/") || listing) return undefined;
    let current = true;
    loadCommands(session.id).then((value) => { if (current) setListing(value); }, () => {});
    return () => { current = false; };
  }, [session.id, ready, draft.text.startsWith("/")]);
  useLayoutEffect(() => { if (listOpen) document.getElementById(`slash-${active}`)?.scrollIntoView({ block: "nearest" }); }, [active, listOpen]);
  const changed = (text) => {
    draft.text = text;
    keepDraft(session.id);
    setActive(0);
    renew((count) => count + 1);
  };
  const pick = (command) => {
    const text = `/${command.name} `;
    field.current.value = text;
    changed(text);
    field.current.focus();
    field.current.setSelectionRange(text.length, text.length);
  };

  // A tap anywhere on the composer card, outside its buttons, puts the caret in the field.
  const focusField = (event) => {
    if (event.target.closest("button, textarea, input") || !ready) return;
    event.preventDefault();
    field.current.focus();
  };
  useLayoutEffect(() => { field.current.value = draft.text; grow(field.current); }, [session.id]);
  // A composer that goes away with the focus gives the other fields back.
  useEffect(() => release, []);

  const send = async () => {
    const body = field.current.value.trim();
    if (busy || !ready || draft.uploads.length || (!body && !draft.attachments.length)) return;
    setBusy(true);
    setError("");
    try {
      // A queued message shows in the queue; a delivered one waits for the saved conversation.
      const answer = await post(session.id, body, draft.attachments);
      draft.text = "";
      draft.attachments = [];
      keepDraft(session.id);
      field.current.value = "";
      grow(field.current);
      onSent(answer);
    } catch (failure) {
      // The draft stays in the field; nothing was sent.
      setError(failure.message);
    }
    setBusy(false);
  };
  const upload = (files) => {
    setError("");
    uploadFiles(files, draft, () => { keepDraft(session.id); renew((count) => count + 1); }, setError);
  };
  const keydown = (event) => {
    if (event.isComposing || event.keyCode === 229) return;
    if (listOpen) {
      const move = { ArrowDown: 1, ArrowUp: -1 }[event.key];
      if (move && matches.length) {
        event.preventDefault();
        setActive((active + move + matches.length) % matches.length);
        return;
      }
      if ((event.key === "Tab" || event.key === "Enter") && !event.shiftKey && matches[active]) {
        event.preventDefault();
        pick(matches[active]);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setClosed(draft.text);
        return;
      }
    }
    // Backspace right after the token removes the whole command.
    const node = field.current;
    if (event.key === "Backspace" && token && node.selectionStart === token.length && node.selectionEnd === token.length) {
      event.preventDefault();
      node.value = draft.text.slice(token.length);
      node.setSelectionRange(0, 0);
      changed(node.value);
      grow(field.current);
      return;
    }
    // On a hardware keyboard Enter sends and Shift+Enter is a newline. On a touch
    // keyboard Enter is a newline. A key that belongs to an IME composition never sends.
    if (event.key !== "Enter" || event.shiftKey || touch()) return;
    event.preventDefault();
    send();
  };
  return html`
    <div class="composer">
      <${FileChips} files=${draft}
        onRemove=${(item) => { draft.attachments = draft.attachments.filter((other) => other !== item); keepDraft(session.id); renew((count) => count + 1); }} />
      ${error && html`<p class="error" role="alert">${error}</p>`}
      ${listOpen && html`<${CommandList} listing=${listing} matches=${matches} active=${active} onPick=${pick} onHover=${setActive} />`}
      <div class="composer-row" onPointerDown=${focusField}>
        <${AttachButton} onFiles=${upload} />
        <div class="composer-input" data-token=${token ? "" : null}>
        ${token && html`<div class="composer-mirror" ref=${mirror} aria-hidden="true"><span class="composer-token">${token}</span>${draft.text.slice(token.length)}${"\u200b"}</div>`}
        <textarea class="composer-field" ref=${field} rows="1" aria-label="Message"
          onScroll=${() => { if (mirror.current) mirror.current.scrollTop = field.current.scrollTop; }}
          placeholder=${ready ? "Message" : "Cannot receive messages"}
          disabled=${!ready} role="combobox" aria-autocomplete="list" aria-expanded=${listOpen}
          aria-controls=${listOpen ? "slash-list" : null} aria-activedescendant=${listOpen && matches.length ? `slash-${active}` : null}
          onInput=${() => { changed(field.current.value); grow(field.current); }} onKeyDown=${keydown}
          onFocus=${isolate} onBlur=${release}></textarea>
        </div>
        <button class="button button-primary composer-send" type="button" aria-label=${working ? "Queue" : "Send"}
          disabled=${busy || !ready || draft.uploads.length > 0} onClick=${send}>
          <span class="send-mark" aria-hidden="true"><${Icon} name="send" /></span><span class="send-text">${working ? "Queue" : "Send"}</span>
        </button>
      </div>
    </div>`;
}

const ANSWER_KEYS = [["enter", "Enter", "Enter"], ["escape", "Esc", "Escape"], ["up", "↑", "Up"], ["down", "↓", "Down"]];

// A blocked agent's own dialog, as it is on its screen, and the keys that answer it.
// ^ No choice is marked or filled: an approval must be a deliberate tap. An answer is
// bound to the screen it was read from; the server refuses it when the dialog changed.
function Question({ session, question, onAnswered }) {
  const [sent, setSent] = useState(null);
  const [busy, setBusy] = useState(false);
  const [typing, setTyping] = useState(false);
  const [error, setError] = useState("");
  const field = useRef();
  const waiting = busy || sent === question.stamp;
  const answer = async (key, text) => {
    if (waiting) return;
    setBusy(true);
    setError("");
    try {
      await api(`api/chat/${session.id}/answer`, { method: "POST", json: { stamp: question.stamp, key, ...(text === undefined ? {} : { text }) } });
      setSent(question.stamp);
      setTyping(false);
    } catch (failure) {
      setError(failure.code === "question_changed" ? "The question changed. Read it again." : failure.message);
    }
    setBusy(false);
    onAnswered();
  };
  const submit = (event) => {
    event.preventDefault();
    const text = field.current.value.trim();
    if (text) answer("text", text);
  };
  useEffect(() => { if (typing) field.current.focus(); }, [typing]);
  return html`
    <section class="question" aria-label="The agent asks">
      ${question.request && html`<h2 class="question-title">${question.request.title}</h2>`}
      <pre class="question-screen" tabindex="0" aria-label="The agent's screen">${question.screen.join("\n")}</pre>
      ${question.options.length ? html`
        <div class="question-options">
          ${question.options.map((option) => html`
            <button key=${option.key} class="button question-option" type="button" disabled=${waiting} onClick=${() => answer(option.key)}>
              <span class="question-key">${option.key}</span><span class="question-label">${option.label}</span>
            </button>`)}
        </div>` : null}
      ${typing && html`
        <form class="question-text" onSubmit=${submit}>
          <input class="input" ref=${field} type="text" maxlength="2000" aria-label="Answer" autocomplete="off" disabled=${waiting} />
          <button class="button" type="submit" disabled=${waiting}>Send</button>
        </form>`}
      ${error && html`<p class="error" role="alert">${error}</p>`}
      <div class="question-keys">
        ${sent === question.stamp ? html`<span class="note" role="status">Sent</span>` : null}
        ${ANSWER_KEYS.map(([key, label, name]) => html`
          <button key=${key} class="button question-keycap" type="button" aria-label=${name} disabled=${waiting} onClick=${() => answer(key)}>${label}</button>`)}
        <button class="link-action" type="button" aria-expanded=${typing} onClick=${() => setTyping(!typing)}>Type an answer</button>
        ${shows("terminal") && can("terminal_view") && html`<a class="link-action" href=${`#/s/${session.id}/terminal`}>Open terminal</a>`}
      </div>
    </section>`;
}

export function Chat({ session, onModel }) {
  const id = session.id;
  const [data, setData] = useState(null);
  // ^ The model of the newest reply, or of a switch the person made, for the top row.
  useEffect(() => { if (data?.model) onModel?.(data.model); }, [data?.model]);
  const [failure, setFailure] = useState("");
  // New items keep the reader at the end only when the reader is at the end.
  const { log, atEnd, away, scrolled, toEnd } = useFollowEnd();
  const generation = useRef(0);
  // The timeline the page shows, one request at a time, and whether a change came during it.
  const shown = useRef(null);
  const loading = useRef(false);
  const again = useRef(false);
  const draft = draftFor(id);

  // ^ While an agent writes, a change comes several times a second. Each one used to fetch
  // the whole timeline (about 200 KB on a long conversation). Now the page asks from the
  // start of its last turn; the server sends the full timeline only when that item is gone.
  const load = useCallback(async () => {
    if (loading.current) {
      again.current = true;
      return;
    }
    loading.current = true;
    const mine = generation.current;
    try {
      do {
        again.current = false;
        const known = shown.current;
        const turn = known?.items.findLast((item) => item.kind === "user" && item.at_unix_ms != null);
        const next = await api(`api/chat/${id}/timeline`, { query: turn ? { from: turn.id, at: turn.at_unix_ms } : undefined });
        if (mine !== generation.current) return;
        if (next.from != null) {
          const kept = known.items.findIndex((item) => item.id === next.from);
          next.items = known.items.slice(0, kept).concat(next.items).slice(-TIMELINE_ITEMS);
        }
        shown.current = next;
        setData(next);
        setFailure("");
      } while (again.current);
    } catch (error) {
      if (mine === generation.current) setFailure(error.message);
    } finally {
      if (mine === generation.current) loading.current = false;
    }
  }, [id]);
  useEffect(() => {
    load();
    const changed = (event) => { if (event.detail.session_id === undefined || event.detail.session_id === id) load(); };
    events.addEventListener("history", changed);
    events.addEventListener("exchange", changed);
    return () => {
      // An answer for the session that was left is dropped, and the next one starts fresh.
      generation.current += 1;
      shown.current = null;
      loading.current = false;
      again.current = false;
      events.removeEventListener("history", changed);
      events.removeEventListener("exchange", changed);
    };
  }, [id]);
  // The session list says first that the agent started or stopped working.
  const status = stateOf(session);
  useEffect(() => { if (data) load(); }, [status.key]);


  const act = async (path, method = "POST") => {
    try {
      await api(`api/chat/${id}/${path}`, { method, json: {} });
      setFailure("");
    } catch (error) {
      setFailure(error.message);
    }
    load();
  };

  const items = data?.items ?? [];
  // ^ Typed into the agent and not in its conversation yet: they end the conversation as
  // sent messages. The rest of the queue waits above the composer.
  const queue = data?.queue ?? [];
  const sent = queue.filter((queued) => queued.delivered_unix_ms);
  const waiting = queue.filter((queued) => !queued.delivered_unix_ms);
  const working = data?.working ?? null;
  const now = useNow(Boolean(working));
  const list = blocks(items);
  const delivery = data?.exchange && !working ? DELIVERY_WORDS[data.exchange.state]
    ?? (data.exchange.evidence === "intent_persisted" && data.exchange.active ? "Saved, not delivered to the agent yet." : null) : null;
  // A blocked agent shows its question where the composer was, or, on a site that may not
  // answer from the chat, a row that leads to the terminal. The draft stays for later.
  const blocked = status.key === "blocked";
  const lastUserBlock = list.findLast?.((block) => block.kind === "user");

  return html`
    <div class="chat">
      <div class="log-wrap">
      <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label="Conversation" tabindex="0">
        ${data?.truncated && html`<p class="note">Older messages are not shown.</p>`}
        ${data && !list.length && !sent.length ? html`<p class="empty">${EMPTY_WORDS[data.status] ?? "No messages yet."}</p>` : null}
        ${list.map((block, index) => block.steps
          ? html`<${Steps} key=${block.id} items=${block.steps} live=${Boolean(working) && index === list.length - 1} />`
          : block.kind === "summary" ? html`<${Summary} key=${block.id} id=${`summary-${block.id}`} text=${block.text} />`
          : block.kind === "notice" ? html`<p key=${block.id} class="note chat-notice" role="status">${block.text}</p>`
          : html`
            <article key=${block.id} class=${block.kind === "user" ? "message message-user" : "message"} data-kind=${block.kind}>
              ${block.text && (block.kind === "user" ? html`<p class="prose">${block.text}</p>`
                : html`<${Markdown} text=${block.text} files=${block.attachments} />`)}
              <${Attachments} files=${block.attachments} />
              ${block === lastUserBlock && delivery && !sent.length ? html`<p class="note" role="status">${delivery}</p>` : null}
            </article>`)}
        ${sent.map((queued) => html`
          <article key=${`sent-${queued.id}`} class="message message-user" data-pending>
            ${queued.text && html`<p class="prose">${queued.text}</p>`}
            <${Attachments} files=${queued.attachments} />
            ${queued.text && html`<${CopyText} text=${queued.text} label="Copy the sent message" />`}
            ${can("chat_send") && html`<button class="link-action" type="button" onClick=${() => act(`queue/${queued.id}/retry`)}>Send again</button>`}
          </article>`)}
        ${working && html`
          <div class="working" role="status">
            <span class="pulse" aria-hidden="true"></span>
            <span class="working-activity">${working.activity}</span>
            ${working.since_unix_ms != null && html`<span class="time">${elapsed(now - working.since_unix_ms)}</span>`}
            ${can("chat_send") && html`<button class="link-action" type="button" onClick=${() => act("cancel")}>Stop</button>`}
          </div>`}
      </div>
      ${away && html`<button class="jump" type="button" onClick=${toEnd}><${Icon} name="down" />Jump to latest</button>`}
      </div>
      ${failure && html`<p class="error chat-error" role="alert">${failure}</p>`}
      ${waiting.length ? html`
        <ul class="queue" aria-label="Messages waiting for the agent">
          ${waiting.map((queued) => html`
            <li key=${queued.id} class="queued" data-failed=${queued.failed ? "" : null}>
              <span class="queued-text">${queued.text || `${queued.attachments.length} files`}</span>
              ${queued.text && html`<${CopyText} text=${queued.text} label=${`Copy waiting message: ${queued.text.slice(0, 40)}`} />`}
              ${queued.failed && html`<button class="link-action" type="button"
                onClick=${() => act(`queue/${queued.id}/retry`)}>Send again</button>`}
              <button class="link-action" type="button" aria-label=${`Remove waiting message: ${queued.text.slice(0, 40)}`}
                onClick=${() => act(`queue/${queued.id}`, "DELETE")}>Remove</button>
              ${queued.failed && html`<p class="queued-reason" role="alert">Not delivered to the agent: ${inWords(queued.failed)}</p>`}
            </li>`)}
        </ul>` : null}
      ${blocked && data?.question ? html`<${Question} session=${session} question=${data.question} onAnswered=${load} />`
      : blocked ? html`
        <div class="attention" role="status">
          <span>Waiting for your answer.</span>
          ${shows("terminal") && can("terminal_view") && html`<a class="link-action" href=${`#/s/${id}/terminal`}>Open terminal</a>`}
        </div>`
      : can("chat_send") && html`<${Composer} session=${session} working=${Boolean(working) || waiting.some((queued) => !queued.failed)}
        onSent=${() => { atEnd.current = true; load(); }} />`}
    </div>`;
}
