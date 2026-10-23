// Inbox: every session by what it needs: your answer, nothing yet, a look, or nothing at all.
// The home screen without task_view; with it, the Sessions tab (#/sessions) beside home.
import { api } from "./api.js";
import { elapsed, html, Icon, useNow, useState } from "./lib.js";
import { can, isNew, listedSessions, sessionLink, shows, stateOf, useStore } from "./store.js";
import { toast } from "./ui.js";

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
  const [removing, setRemoving] = useState(false);
  // ^ Slide the row left to show Remove (docs/web-client.md "Screens"). The slide
  // is the confirmation: one stray tap removes nothing. The row goes when wsxd says so.
  const remove = async () => {
    setRemoving(true);
    try {
      await api(`api/sessions/${session.id}`, { method: "DELETE" });
    } catch (failure) {
      toast(failure.message, { failure: true });
      setRemoving(false);
    }
  };
  // ^ The owner's hand session of a task closes with its handback (docs/tasks.md "By hand").
  const removable = can("session_start") && !/^task-\d+-hand$/.test(session.label);
  // Remove is filled only while the row is slid open, so closed rows show no red buttons.
  const slid = (event) => event.currentTarget.toggleAttribute("data-open", event.currentTarget.scrollLeft > 0);
  return html`
    <li class=${removable ? "row row-slide" : "row"} onScroll=${removable ? slid : null}>
      <a class="row-link" href=${sessionLink(session)} aria-current=${current ? "page" : null}>
        <span class="mark" data-tone=${status.tone} data-running=${status.key === "working" ? "" : null} aria-hidden="true"></span>
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
      ${removable && html`<button class="row-remove" type="button" disabled=${removing} onClick=${remove}
        aria-label=${`Remove session ${session.label}`} title="Remove"><${Icon} name="fail" /></button>`}
    </li>`;
}

// Starts a session in `worktree`, or in the project's main checkout without one.
// A + icon; with `label`, a text action inside a sentence ("Start one").
export function NewSession({ project, worktree, label }) {
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  if (!can("session_start")) return null;
  // ^ The name says what the + does: a session, not a project or a worktree.
  const name = `New session in ${worktree ? worktree.branch || "detached" : project.name}`;
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
    ${label
      ? html`<button class="link-action" type="button" disabled=${busy} onClick=${create} aria-label=${name}>${label}</button>`
      : html`<button class="icon-button new-session" type="button" disabled=${busy} onClick=${create} aria-label=${name} title=${name}><${Icon} name="plus" /></button>`}
    ${error && html`<span class="error" role="alert">${error}</span>`}`;
}

const SECTIONS = [["needs", "Needs you"], ["running", "Running"], ["finished", "Finished"]];

export function Inbox({ rail }) {
  const store = useStore();
  // Every session but task workers and the manager, which their task and their project's
  // chat show. A shell has no state group: it is listed with its project.
  const sessions = listedSessions(store.sessions);
  const now = useNow(sessions.some((session) => stateOf(session).key === "working"));
  const currentId = Number(location.hash.split("/")[2]) || null;
  const row = (session, place) => html`<${SessionRow} key=${session.id} session=${session} now=${now} current=${rail && session.id === currentId} place=${place} />`;
  const grouped = (group) => sessions.filter((session) => stateOf(session).group === group);
  // ^ The newest finish first, by when the session reached its state. Seeing a session
  // never changes that time, so opening one and coming back moves no row.
  const finished = grouped("finished").sort((left, right) =>
    right.state_since_unix_ms - left.state_since_unix_ms || left.id - right.id);
  const rest = sessions.filter((session) => [null, "rest"].includes(stateOf(session).group));
  const projects = store.projects.filter((project) => rest.some((session) => session.project.id === project.id));
  let body;
  if (!store.daemon) {
    body = html`<p class="empty">wsx is not running on the host. Sessions return when it starts.</p>`;
  } else if (!store.loaded) {
    body = html`<p class="empty">Loading…</p>`;
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
  const top = !rail && html`<h1 class="title">${can("task_view") ? "Sessions" : "Inbox"}</h1>`;
  return html`
    <div class="page">
      ${top}
      <div class="scroll">${body}</div>
    </div>`;
}
