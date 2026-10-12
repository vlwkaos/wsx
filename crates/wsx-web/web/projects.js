// Projects: every project this site may reach, with its files and a new session.
import { html } from "./lib.js";
import { can, shows, useStore } from "./store.js";
import { NewSession } from "./inbox.js";

export function Projects({ rail }) {
  const store = useStore();
  const files = shows("files") && can("files_read");
  const count = (project) => store.sessions.filter((session) => session.project.id === project.id).length;
  let body;
  if (!store.daemon) body = html`<p class="empty">wsx is not running on the host.</p>`;
  else if (!store.loaded) body = html`<p class="empty">Loading…</p>`;
  else if (!store.projects.length) body = html`<p class="empty">wsx manages no project that this site may reach.</p>`;
  else {
    body = html`
      <ul class="rows">
        ${store.projects.map((project) => html`
          <li class="row" key=${project.id}>
            <div class="row-link">
              <span class="row-main">
                <span class="row-label">${project.name}</span>
                <span class="row-context">${count(project) === 1 ? "1 session" : `${count(project)} sessions`}</span>
              </span>
              <span class="row-actions">
                ${files && html`<a class="link-action" href=${`#/p/${project.id}/files`} aria-label=${`Files of ${project.name}`}>Files</a>`}
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
