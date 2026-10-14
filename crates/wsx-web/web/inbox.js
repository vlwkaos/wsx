// Inbox: the home screen. Agent state is the first thing to read, so sessions are
// listed by what they need: your answer, nothing yet, a look, or nothing at all.
import { api } from "./api.js";
import { elapsed, html, useNow, useState } from "./lib.js";
import { can, isAgent, isNew, sessionLink, shows, stateOf, useStore } from "./store.js";

// One session. Left: a state mark. Middle: the work first, project › worktree, then the
// session and its provider, and for a working session what it does. Right: the state
// word and, while working, the time. Each level has one look (docs/web-client.md "Screens").
// `place` drops what a heading above already names: "project" (the inbox's project
// groups) or "worktree" (a project's worktree list).
export function SessionRow({ session, now, current, place }) {
  const status = stateOf(session);
  const fresh = status.group === "finished" && isNew(session);
  const branch = session.branch || "detached";
  let identity;
  if (place === "worktree") identity = html`<span class="row-session-name">${session.label}</span>`;
  else if (place === "project") identity = html`<span class="row-worktree">${branch}</span>`;
  else identity = html`<span class="row-project">${session.project.name}</span><span class="row-sep" aria-hidden="true">›</span><span class="row-worktree">${branch}</span>`;
  const context = [place !== "worktree" && session.label, session.agent?.provider ?? "shell"].filter(Boolean).join(" · ");
  return html`
    <li class="row">
      <a class="row-link" href=${sessionLink(session)} aria-current=${current ? "page" : null}>
        <span class="mark" data-tone=${status.tone} aria-hidden="true"></span>
        <span class="row-main">
          <span class="row-work">${identity}</span>
          ${context && html`<span class="row-context">${context}</span>`}
          ${status.key === "working" && session.activity && html`<span class="row-activity">${session.activity}</span>`}
        </span>
        <span class="row-state" data-tone=${status.tone}>
          <span>${status.word}${fresh ? html`${" "}<span class="new">new</span>` : null}</span>
          ${status.key === "working" && session.state_since_unix_ms != null && html`<span class="time">${elapsed(now - session.state_since_unix_ms)}</span>`}
        </span>
      </a>
    </li>`;
}

// Starts a session in `worktree`, or in the project's main checkout without one.
export function NewSession({ project, worktree, label = "New session" }) {
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  if (!can("session_start")) return null;
  const create = async () => {
    setBusy(true);
    setError("");
    try {
      const created = await api("api/sessions", { method: "POST", json: { project: project.id, worktree: worktree?.id, label: "chat" } });
      location.hash = `#/s/${created.session_id}/${shows("chat") ? "chat" : "terminal"}`;
    } catch (failure) {
      setError(failure.message);
    }
    setBusy(false);
  };
  return html`
    <button class="link-action" type="button" disabled=${busy} onClick=${create}
      aria-label=${`New session in ${project.name}${worktree ? ` ${worktree.branch}` : ""}`}>${label}</button>
    ${error && html`<span class="error" role="alert">${error}</span>`}`;
}

const SECTIONS = [["needs", "Needs you"], ["running", "Running"], ["finished", "Finished"]];

export function Inbox({ rail }) {
  const store = useStore();
  // The inbox is agent work; shells are listed under their worktree in Projects.
  const sessions = store.sessions.filter(isAgent);
  const now = useNow(sessions.some((session) => stateOf(session).key === "working"));
  const currentId = Number(location.hash.split("/")[2]) || null;
  const row = (session, place) => html`<${SessionRow} key=${session.id} session=${session} now=${now} current=${rail && session.id === currentId} place=${place} />`;
  const grouped = (group) => sessions.filter((session) => stateOf(session).group === group);
  // ^ The newest finish first, by when the session reached its state. Seeing a session
  // never changes that time, so opening one and coming back moves no row.
  const finished = grouped("finished").sort((left, right) =>
    right.state_since_unix_ms - left.state_since_unix_ms || left.id - right.id);
  const rest = grouped("rest");
  const projects = store.projects.filter((project) => rest.some((session) => session.project.id === project.id));
  let body;
  if (!store.daemon) {
    body = html`<p class="empty">wsx is not running on the host. Sessions return when it starts.</p>`;
  } else if (!store.loaded) {
    body = html`<p class="empty">Loading…</p>`;
  } else if (!sessions.length && store.sessions.length) {
    body = html`<p class="empty">No agent session. Shells are in <a class="link-action" href="#/projects">Projects</a>.</p>`;
  } else if (!sessions.length) {
    body = html`
      <p class="empty">No session is open.
        ${store.projects.length === 1 ? html`${" "}<${NewSession} project=${store.projects[0]} label="Start one" />`
          : can("session_start") && store.projects.length ? html`${" "}<a class="link-action" href="#/projects">Start one in a project</a>` : null}
      </p>`;
  } else {
    body = html`
      ${SECTIONS.map(([group, title]) => {
        const list = group === "finished" ? finished : grouped(group);
        return list.length ? html`
          <section key=${group} aria-label=${title}>
            <h2 class="label">${title}</h2>
            <ul class="rows">${list.map((session) => row(session))}</ul>
          </section>` : null;
      })}
      ${projects.length ? html`
        <section aria-label="Sessions">
          <h2 class="label">Sessions</h2>
          ${projects.map((project) => html`
            <div class="group" key=${project.id}>
              <div class="group-head"><span class="group-name">${project.name}</span><${NewSession} project=${project} /></div>
              <ul class="rows">${rest.filter((session) => session.project.id === project.id).map((session) => row(session, "project"))}</ul>
            </div>`)}
        </section>` : null}`;
  }
  return html`
    <div class="page">
      ${!rail && html`<h1 class="title">Inbox</h1>`}
      <div class="scroll">${body}</div>
    </div>`;
}
