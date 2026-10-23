// A session owns the full height: one top row (back, label with its state in words,
// Chat | Terminal), then the chat or the terminal.
import { html, Loading, useEffect, useRef, useState } from "./lib.js";
import { can, isAgent, markSeen, replaceRoute, sessionLink, shows, stateOf, useStore } from "./store.js";
import { Back } from "./ui.js";
import { Chat } from "./chat.js";
import { terminalView } from "./terminal.js";

// The existing imperative terminal, mounted into this screen. Its size toggle goes
// into the top row.
function Terminal({ session, tools }) {
  const mount = useRef();
  useEffect(() => terminalView(mount.current, session, tools.current).close, [session]);
  return html`<div class="terminal-mount" ref=${mount}></div>`;
}

export function Session({ id, tab }) {
  const store = useStore();
  const tools = useRef();
  // Kept while the terminal is shown: only the chat reads the saved conversation.
  const [model, setModel] = useState(null);
  const session = store.sessions.find((candidate) => candidate.id === id);
  // A shell has no chat; it becomes one when an agent starts in it, and keeps it after the
  // agent exits while the server remembers the conversation.
  const tabs = [session && (isAgent(session) || session.conversation) && shows("chat") && "chat", shows("terminal") && can("terminal_view") && "terminal"].filter(Boolean);
  const active = tabs.includes(tab) ? tab : tabs[0];
  useEffect(() => { if (session && active === "chat") markSeen(session); });
  if (!session) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback="#/" /></header>
        ${store.daemon && !store.loaded ? html`<${Loading} />` : html`<p class="empty">${!store.daemon ? "wsx is not running on the host." : "This session is closed."}</p>`}
      </div>`;
  }
  const status = stateOf(session);
  return html`
    <div class="page session">
      <header class="bar">
        <${Back} fallback="#/" />
        <div class="bar-title">
          <span class="bar-label">${session.label}</span>
          <span class="bar-state" data-tone=${status.tone}>${status.key === "working" && html`<span class="pulse" aria-hidden="true"></span>`}${status.word}${model && html`<span class="bar-model" title="Model"> · ${model}</span>`}</span>
        </div>
        <div class="bar-tools" ref=${tools}></div>
        ${tabs.length > 1 && html`
          <nav class="switch" aria-label="View">
            ${tabs.map((name) => html`
              <a key=${name} href=${sessionLink(session, name)} aria-current=${active === name ? "page" : null}
                onClick=${(event) => replaceRoute(event, sessionLink(session, name))}>
                ${name === "chat" ? "Chat" : "Terminal"}</a>`)}
          </nav>`}
      </header>
      ${active === "terminal" ? html`<${Terminal} key="terminal" session=${id} tools=${tools} />`
        : active === "chat" ? html`<${Chat} key="chat" session=${session} onModel=${setModel} />`
        : html`<p class="empty">This session is a shell, and this site does not show terminals.</p>`}
    </div>`;
}
