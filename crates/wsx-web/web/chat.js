// Chat: the agent's own saved conversation as a typed timeline, what the agent does
// now, the messages that wait for it, and the composer (docs/web-client.md "Chat").
import { api, resolve } from "./api.js";
import { bytes, elapsed, html, Icon, useCallback, useEffect, useLayoutEffect, useNow, useRef, useState } from "./lib.js";
import { can, events, shows, stateOf, state, touch } from "./store.js";

// Unsent text, attachments and the message that is not in the saved conversation
// yet, per session. Kept outside the view so a route change does not lose them.
const drafts = new Map();
const draftFor = (id) => {
  if (!drafts.has(id)) drafts.set(id, { text: "", attachments: [], pending: null });
  return drafts.get(id);
};

const EMPTY_WORDS = {
  no_agent: "No agent runs in this session.",
  identity_unavailable: "The agent has not reported its conversation yet.",
  unsupported_provider: "This agent type has no readable saved conversation.",
  history_not_found: "The saved conversation file was not found.",
  history_unreadable: "No messages yet.",
  no_readable_messages: "No messages yet.",
};
// Exchange states where the last sent message did not go the expected way.
const DELIVERY_WORDS = {
  delivery_failed: "Not delivered to the agent.",
  cancelled: "Stopped.",
  cancel_requested: "Stopping…",
  expired: "The agent did not finish in time.",
  interrupted: "The agent went away before it finished.",
  target_exited: "The agent's program exited.",
  target_replaced: "The agent's program was replaced.",
};

const Attachments = ({ files }) => files?.length ? html`
  <ul class="attachments">
    ${files.map((file) => html`
      <li key=${file.hash}>
        ${file.image
          ? html`<a href=${resolve(file.url)}><img src=${resolve(file.url)} alt=${file.name} loading="lazy" /></a>`
          : html`<a href=${resolve(file.url)} download=${file.name}>${file.name} (${bytes(file.size)})</a>`}
      </li>`)}
  </ul>` : null;

const stepWord = (item) => item.kind === "thinking" ? "Thinking" : item.tool.name;
const Step = ({ item }) => html`
  <li class="step">
    <span class="step-title">${item.kind === "thinking" ? "Thinking" : item.tool.title}</span>
    ${item.tool?.status === "error" ? html`<span class="step-state" data-tone="danger">failed</span>`
      : item.tool?.status === "running" ? html`<span class="step-state">running</span>` : null}
  </li>`;

// Consecutive tool and thinking items between two messages are one line. The run that
// is still going shows its last three items.
function Steps({ items, live }) {
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

function Composer({ session, working, onSent }) {
  const draft = draftFor(session.id);
  const [, renew] = useState(0);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const field = useRef();
  const picker = useRef();
  const blocked = stateOf(session).key === "blocked";
  const ready = Boolean(session.agent?.attached && session.agent.prompt) && !session.exited;

  // The field grows with its text, up to six lines.
  const grow = () => {
    const node = field.current;
    node.style.height = "auto";
    const line = parseFloat(getComputedStyle(node).lineHeight);
    node.style.height = `${Math.min(node.scrollHeight, line * 6 + (node.offsetHeight - node.clientHeight))}px`;
  };
  useLayoutEffect(() => { field.current.value = draft.text; grow(); }, [session.id]);

  const send = async () => {
    const body = field.current.value.trim();
    if (busy || blocked || !ready || (!body && !draft.attachments.length)) return;
    setBusy(true);
    setError("");
    try {
      const answer = await api(`api/chat/${session.id}/send`, {
        method: "POST", json: { text: body, attachments: draft.attachments.map((item) => item.hash) },
      });
      // A queued message shows in the queue; a delivered one waits for the saved conversation.
      if (!answer.queued) draft.pending = { text: body, attachments: draft.attachments };
      draft.text = "";
      draft.attachments = [];
      field.current.value = "";
      grow();
      onSent(answer);
    } catch (failure) {
      // The draft stays in the field; nothing was sent.
      setError(failure.message);
    }
    setBusy(false);
  };
  const upload = async (files) => {
    picker.current.value = "";
    setError("");
    for (const item of files) {
      if (item.size > state.site.max_file_bytes) {
        setError(`${item.name} is larger than ${bytes(state.site.max_file_bytes)}.`);
        continue;
      }
      try {
        draft.attachments.push(await api("api/media", { method: "PUT", query: { name: item.name }, body: item }));
      } catch (failure) {
        setError(`${item.name}: ${failure.message}`);
      }
      renew((count) => count + 1);
    }
  };
  const keydown = (event) => {
    // On a hardware keyboard Enter sends and Shift+Enter is a newline. On a touch
    // keyboard Enter is a newline. A key that belongs to an IME composition never sends.
    if (event.key !== "Enter" || event.shiftKey || event.isComposing || event.keyCode === 229 || touch()) return;
    event.preventDefault();
    send();
  };
  return html`
    <div class="composer">
      ${draft.attachments.length ? html`
        <ul class="chips" aria-label="Files to send">
          ${draft.attachments.map((item) => html`
            <li key=${item.hash}><span>${item.name} (${bytes(item.size)})</span>
              <button class="link-action" type="button" aria-label=${`Remove ${item.name}`}
                onClick=${() => { draft.attachments = draft.attachments.filter((other) => other !== item); renew((count) => count + 1); }}>Remove</button>
            </li>`)}
        </ul>` : null}
      ${error && html`<p class="error" role="alert">${error}</p>`}
      <div class="composer-row">
        ${can("media_upload") && html`
          <button class="icon-button" type="button" aria-label="Attach files" onClick=${() => picker.current.click()}><${Icon} name="attach" /></button>
          <input type="file" multiple hidden ref=${picker} onChange=${(event) => upload([...event.target.files])} />`}
        <textarea class="composer-field" ref=${field} rows="1" aria-label="Message"
          placeholder=${blocked ? "Answer in the terminal" : ready ? "Message" : "Cannot receive messages"}
          disabled=${!ready || blocked}
          onInput=${() => { draft.text = field.current.value; grow(); }} onKeyDown=${keydown}></textarea>
        <button class="button button-primary" type="button" disabled=${busy || !ready || blocked} onClick=${send}>${working ? "Queue" : "Send"}</button>
      </div>
    </div>`;
}

export function Chat({ session }) {
  const id = session.id;
  const [data, setData] = useState(null);
  const [failure, setFailure] = useState("");
  const [away, setAway] = useState(false);
  const log = useRef();
  const atEnd = useRef(true);
  const generation = useRef(0);
  const draft = draftFor(id);

  const load = useCallback(async () => {
    const mine = ++generation.current;
    try {
      const next = await api(`api/chat/${id}/timeline`);
      // A slower earlier answer must not replace a newer one.
      if (mine !== generation.current) return;
      setData(next);
      setFailure("");
    } catch (error) {
      if (mine === generation.current) setFailure(error.message);
    }
  }, [id]);
  useEffect(() => {
    load();
    const changed = (event) => { if (event.detail.session_id === undefined || event.detail.session_id === id) load(); };
    events.addEventListener("history", changed);
    events.addEventListener("exchange", changed);
    return () => {
      generation.current += 1;
      events.removeEventListener("history", changed);
      events.removeEventListener("exchange", changed);
    };
  }, [id]);
  // The session list says first that the agent started or stopped working.
  const status = stateOf(session);
  useEffect(() => { if (data) load(); }, [status.key]);

  // New items keep the reader at the end only when the reader is at the end.
  const scrolled = () => {
    const node = log.current;
    // A scroll event may still arrive after the view closed.
    if (!node) return;
    atEnd.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
    setAway(!atEnd.current);
  };
  const toEnd = () => { log.current.scrollTop = log.current.scrollHeight; atEnd.current = true; setAway(false); };
  useLayoutEffect(() => { if (atEnd.current && log.current) log.current.scrollTop = log.current.scrollHeight; });

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
  const lastUser = items.filter((item) => item.kind === "user").at(-1);
  // The saved conversation now holds the sent message: stop showing it as pending.
  if (draft.pending && lastUser?.text === draft.pending.text) draft.pending = null;
  const working = data?.working ?? null;
  const now = useNow(Boolean(working));
  const list = blocks(items);
  const delivery = data?.exchange && !working ? DELIVERY_WORDS[data.exchange.state]
    ?? (data.exchange.evidence === "intent_persisted" && data.exchange.active ? "Saved, not delivered to the agent yet." : null) : null;
  const lastUserBlock = list.findLast?.((block) => block.kind === "user");

  return html`
    <div class="chat">
      <div class="log-wrap">
      <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label="Conversation" tabindex="0">
        ${data?.checkpoint ? html`<details class="steps"><summary class="steps-summary"><${Icon} name="chevron" />Earlier conversation (summary)</summary><p class="prose">${data.checkpoint}</p></details>`
          : data?.truncated ? html`<p class="note">Older messages are not shown.</p>` : null}
        ${data && !list.length && !draft.pending ? html`<p class="empty">${EMPTY_WORDS[data.status] ?? "No messages yet."}</p>` : null}
        ${list.map((block, index) => block.steps
          ? html`<${Steps} key=${block.id} items=${block.steps} live=${Boolean(working) && index === list.length - 1} />`
          : html`
            <article key=${block.id} class=${block.kind === "user" ? "message message-user" : "message"} data-kind=${block.kind}>
              ${block.text && html`<p class="prose">${block.text}</p>`}
              <${Attachments} files=${block.attachments} />
              ${block === lastUserBlock && delivery && !draft.pending ? html`<p class="note" role="status">${delivery}</p>` : null}
            </article>`)}
        ${draft.pending && html`
          <article class="message message-user" data-pending>
            ${draft.pending.text && html`<p class="prose">${draft.pending.text}</p>`}
            <${Attachments} files=${draft.pending.attachments} />
          </article>`}
        ${working && html`
          <div class="working" role="status">
            <span class="pulse" aria-hidden="true"></span>
            <span class="working-activity">${working.activity}</span>
            <span class="time">${elapsed(now - working.since_unix_ms)}</span>
            ${can("chat_send") && html`<button class="link-action" type="button" onClick=${() => act("cancel")}>Stop</button>`}
          </div>`}
        ${status.key === "blocked" && html`
          <div class="attention" role="status">
            <span>The agent is waiting for your answer.</span>
            ${shows("terminal") && can("terminal_view") && html`<a class="link-action" href=${`#/s/${id}/terminal`}>Open terminal</a>`}
          </div>`}
      </div>
      ${away && html`<button class="jump" type="button" onClick=${toEnd}><${Icon} name="down" />Jump to latest</button>`}
      </div>
      ${failure && html`<p class="error chat-error" role="alert">${failure}</p>`}
      ${data?.queue?.length ? html`
        <ul class="queue" aria-label="Messages waiting for the agent">
          ${data.queue.map((queued) => html`
            <li key=${queued.id} class="queued">
              <span class="queued-text">${queued.text || `${queued.attachments.length} files`}</span>
              ${queued.failed && html`<span class="queued-reason" data-tone="danger">${queued.failed}</span>`}
              ${queued.failed && html`<button class="link-action" type="button" onClick=${() => act(`queue/${queued.id}/retry`)}>Retry</button>`}
              <button class="link-action" type="button" aria-label=${`Remove waiting message: ${queued.text.slice(0, 40)}`}
                onClick=${() => act(`queue/${queued.id}`, "DELETE")}>Remove</button>
            </li>`)}
        </ul>` : null}
      ${can("chat_send") && html`<${Composer} session=${session} working=${Boolean(working) || Boolean(data?.queue?.length)}
        onSent=${() => { atEnd.current = true; load(); }} />`}
    </div>`;
}
