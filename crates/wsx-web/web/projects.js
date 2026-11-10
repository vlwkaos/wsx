// Projects: every project this site may reach. A project opens its worktrees and
// their sessions (with task_view too, not its chat); Files and New session stay on each row.
import { html, Loading, plural, useEffect, useNow, useRef, useState } from "./lib.js";
import { api } from "./api.js";
import { can, listedSessions, shows, stateOf, useStore } from "./store.js";
import { NewSession, SessionRow } from "./inbox.js";
import { Back } from "./ui.js";
import { PanelsButton } from "./panels.js";

const counted = (count) => plural(count, "session");
// How many task items wait for the owner in a project, after its session count.
const waiting = (store, project) => {
  const count = can("task_view") ? store.tasks[project.id]?.needs.length ?? 0 : 0;
  return count ? ` · ${count} ${count === 1 ? "needs" : "need"} you` : "";
};
const filesLink = (project) => shows("files") && can("files_read") && html`
  <a class="link-action" href=${`#/p/${project.id}/files`} aria-label=${`Files of ${project.name}`}>Files</a>`;

export function Projects({ rail }) {
  const store = useStore();
  const count = (project) => listedSessions(store.sessions).filter((session) => session.project.id === project.id).length;
  const [, , open] = location.hash.split("/");
  let body;
  if (!store.daemon) body = html`<p class="empty">wsx is not running on the host.</p>`;
  else if (!store.loaded) body = html`<${Loading} />`;
  else if (!store.projects.length) body = html`<p class="empty">wsx manages no project that this site may reach.</p>`;
  else {
    body = html`
      <ul class="rows">
        ${store.projects.map((project) => html`
          <li class="row" key=${project.id}>
            <div class="row-link" aria-current=${rail && Number(open) === project.id ? "page" : null}>
              <a class="row-main row-open" href=${can("task_view") ? `#/p/${project.id}/sessions` : `#/p/${project.id}`}>
                <span class="row-label">${project.name}</span>
                <span class="row-context">${counted(count(project))}${waiting(store, project)}</span>
              </a>
              <span class="row-actions">
                ${filesLink(project)}
                <${NewSession} project=${project} />
              </span>
            </div>
          </li>`)}
      </ul>`;
  }
  return html`
    <div class="page">
      ${!rail && html`<h1 class="title">Projects</h1>`}
      <div class="scroll">${body}</div>
    </div>`;
}

// A project's sessions, from the manager's top row (#/p/<id>/sessions).
export const ProjectSessions = ({ id }) => html`<${Project} id=${id} back=${`#/p/${id}`} />`;

// ^ The confirmation of a worktree's delete (docs/tasks.md "Stale worktrees"): it asks the server
// what the delete closes and loses, and names it. A worktree with nothing to lose is deleted
// by the plain button; one with uncommitted work or commits neither merged nor pushed needs
// the button that names the loss. A failure stays in the sheet.
function DeleteWorktree({ worktree, onClose }) {
  const dialog = useRef();
  const [facts, setFacts] = useState(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    dialog.current.showModal();
    api(`api/worktrees/${worktree.id}`).then(setFacts, (failure) => setError(failure.message));
  }, []);
  const lost = facts?.lost;
  const submit = async (event) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api(`api/worktrees/${worktree.id}${lost ? "?confirm=true" : ""}`, { method: "DELETE" });
      dialog.current.close();
    } catch (failure) {
      setError(failure.message);
      setBusy(false);
    }
  };
  return html`
    <dialog class="sheet" ref=${dialog} aria-labelledby="delete-worktree-title" onClose=${onClose}>
      <form onSubmit=${submit}>
        <h2 class="sheet-title" id="delete-worktree-title">Delete ${worktree.branch || "detached"}?</h2>
        ${facts && html`<p class="muted">${counted(facts.sessions)} close. ${lost ? html`<strong>It loses ${lost}.</strong>` : "Nothing is lost: it is clean, and merged or pushed."}</p>`}
        <p class="error" role="alert">${error}</p>
        <div class="actions sheet-actions">
          <button class="button" type="button" onClick=${() => dialog.current.close()}>Cancel</button>
          <button class="button button-danger" type="submit" disabled=${busy || !facts}>${lost ? "Delete and lose it" : "Delete"}</button>
        </div>
      </form>
    </dialog>`;
}

// One project: its worktrees, main checkout first, each with its sessions.
export function Project({ id, back = "#/projects" }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const sessions = project ? listedSessions(store.sessions).filter((session) => session.project.id === id) : [];
  const now = useNow(sessions.some((session) => stateOf(session).key === "working"));
  const [deleting, setDeleting] = useState(null);
  if (!project) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback=${back} /></header>
        ${store.daemon && !store.loaded ? html`<${Loading} />` : html`<p class="empty">${!store.daemon ? "wsx is not running on the host." : "This project is not available."}</p>`}
      </div>`;
  }
  const worktrees = [...project.worktrees].sort((left, right) => Number(right.main) - Number(left.main));
  const deletable = can("session_start") && can("files_write");
  return html`
    <div class="page">
      <header class="bar">
        <${Back} fallback=${back} />
        <div class="bar-title">
          <span class="bar-label">${project.name}</span>
          <span class="bar-state">${counted(sessions.length)}</span>
        </div>
        <div class="bar-tools">${filesLink(project)}<${PanelsButton} project=${id} /></div>
      </header>
      <div class="scroll">
        ${worktrees.map((worktree) => {
          const own = sessions.filter((session) => session.worktree === worktree.id);
          return html`
            <section class="group worktree" key=${worktree.id} aria-label=${worktree.branch || "detached"}>
              <div class="group-head">
                <h2 class="group-name"><span class="group-branch">${worktree.branch || "detached"}</span>${worktree.main ? " · main checkout" : null}</h2>
                ${deletable && !worktree.main && html`<button class="link-action delete-worktree" type="button" aria-label=${`Delete worktree ${worktree.branch || "detached"}`}
                  onClick=${() => setDeleting(worktree)}>Delete</button>`}
                <${NewSession} project=${project} worktree=${worktree} />
              </div>
              ${own.length
                ? html`<ul class="rows">${own.map((session) => html`
                    <${SessionRow} key=${session.id} session=${session} now=${now} place="worktree" />`)}</ul>`
                : html`<p class="note">No session.</p>`}
            </section>`;
        })}
      </div>
      ${deleting && html`<${DeleteWorktree} worktree=${deleting} onClose=${() => setDeleting(null)} />`}
    </div>`;
}
