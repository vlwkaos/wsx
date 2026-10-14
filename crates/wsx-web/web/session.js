// A session owns the full height: one top row (back, label with its state in words,
// Chat | Terminal), then the chat or the terminal.
import { html, useEffect, useRef } from "./lib.js";
import { can, markSeen, replaceRoute, sessionLink, shows, stateOf, useStore } from "./store.js";
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
  const session = store.sessions.find((candidate) => candidate.id === id);
  const tabs = [shows("chat") && "chat", shows("terminal") && can("terminal_view") && "terminal"].filter(Boolean);
  const active = tabs.includes(tab) ? tab : tabs[0];
  useEffect(() => { if (session && active === "chat") markSeen(session); });
  if (!session) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback="#/" /></header>
        <p class="empty">${!store.daemon ? "wsx is not running on the host." : store.loaded ? "This session is closed." : "Loading…"}</p>
      </div>`;
  }
  const status = stateOf(session);
  return html`
    <div class="page session">
      <header class="bar">
        <${Back} fallback="#/" />
        <div class="bar-title">
          <span class="bar-label">${session.label}</span>
          <span class="bar-state" data-tone=${status.tone}>${status.word}</span>
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
      ${active === "terminal"
        ? html`<${Terminal} key="terminal" session=${id} tools=${tools} />`
        : html`<${Chat} key="chat" session=${session} />`}
    </div>`;
}
