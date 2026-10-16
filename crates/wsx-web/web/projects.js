// Projects: every project this site may reach. A project opens its worktrees and
// their sessions; Files and New session stay on each row.
import { html, useNow } from "./lib.js";
import { can, listedSessions, shows, stateOf, useStore } from "./store.js";
import { NewSession, SessionRow } from "./inbox.js";
import { Back } from "./ui.js";

const counted = (count) => (count === 1 ? "1 session" : `${count} sessions`);
// How many task items wait for the owner in a project, after its session count.
const waiting = (store, project) => {
  const count = can("task_view") ? store.tasks[project.id]?.needs.length ?? 0 : 0;
  return count ? ` · ${count} ${count === 1 ? "needs" : "need"} you` : "";
};
export const filesLink = (project) => shows("files") && can("files_read") && html`
  <a class="link-action" href=${`#/p/${project.id}/files`} aria-label=${`Files of ${project.name}`}>Files</a>`;

export function Projects({ rail }) {
  const store = useStore();
  const count = (project) => listedSessions(store.sessions).filter((session) => session.project.id === project.id).length;
  const [, , open] = location.hash.split("/");
  let body;
  if (!store.daemon) body = html`<p class="empty">wsx is not running on the host.</p>`;
  else if (!store.loaded) body = html`<p class="empty">Loading…</p>`;
  else if (!store.projects.length) body = html`<p class="empty">wsx manages no project that this site may reach.</p>`;
  else {
    body = html`
      <ul class="rows">
        ${store.projects.map((project) => html`
          <li class="row" key=${project.id}>
            <div class="row-link" aria-current=${rail && Number(open) === project.id ? "page" : null}>
              <a class="row-main row-open" href=${`#/p/${project.id}`}>
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

// One project: its worktrees, main checkout first, each with its sessions.
export function Project({ id, back = "#/projects" }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const sessions = project ? listedSessions(store.sessions).filter((session) => session.project.id === id) : [];
  const now = useNow(sessions.some((session) => stateOf(session).key === "working"));
  if (!project) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback=${back} /></header>
        <p class="empty">${!store.daemon ? "wsx is not running on the host." : store.loaded ? "This project is not available." : "Loading…"}</p>
      </div>`;
  }
  const worktrees = [...project.worktrees].sort((left, right) => Number(right.main) - Number(left.main));
  return html`
    <div class="page">
      <header class="bar">
        <${Back} fallback=${back} />
        <div class="bar-title">
          <span class="bar-label">${project.name}</span>
          <span class="bar-state">${counted(sessions.length)}</span>
        </div>
        <div class="bar-tools">${filesLink(project)}</div>
      </header>
      <div class="scroll">
        ${worktrees.map((worktree) => {
          const own = sessions.filter((session) => session.worktree === worktree.id);
          return html`
            <section class="group worktree" key=${worktree.id} aria-label=${worktree.branch || "detached"}>
              <div class="group-head">
                <h2 class="group-name"><span class="group-branch">${worktree.branch || "detached"}</span>${worktree.main ? " · main checkout" : null}</h2>
                <${NewSession} project=${project} worktree=${worktree} />
              </div>
              ${own.length
                ? html`<ul class="rows">${own.map((session) => html`
                    <${SessionRow} key=${session.id} session=${session} now=${now} place="worktree" />`)}</ul>`
                : html`<p class="note">No session.</p>`}
            </section>`;
        })}
      </div>
    </div>`;
}
