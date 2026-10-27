// The project page: the deploy, the release and the main checkout in words, then one markdown
// file with a mermaid graph that the manager keeps up to date with the Edit tool, shown in the
// sheet over the chat (docs/tasks.md "Project page", docs/web-client-tasks.md "Project page").
import { api } from "./api.js";
import { ago, html, Icon, Loading, useEffect, useNow, useRef, useState } from "./lib.js";
import { events, useStore } from "./store.js";
import { Markdown } from "./markdown.js";
import { withChips } from "./tasks.js";
import { ProjectFacts } from "./story.js";

export function ProjectPage({ project, onClose, grip }) {
  const overview = useStore().tasks[project];
  const [page, setPage] = useState(null);
  const [failure, setFailure] = useState("");
  const live = useRef(true);
  const now = useNow(Boolean(page?.unix_ms));
  useEffect(() => {
    live.current = true;
    const read = () => api(`api/tasks/${project}/page`).then(
      (value) => { if (live.current) { setPage(value); setFailure(""); } },
      (error) => { if (live.current) setFailure(error.message); });
    read();
    const changed = (event) => { if (event.detail.project == null || event.detail.project === project) read(); };
    events.addEventListener("page", changed);
    return () => {
      live.current = false;
      events.removeEventListener("page", changed);
    };
  }, [project]);
  return html`
    <div class="page task-page">
      <header class="bar" onPointerDown=${grip?.down} onPointerMove=${grip?.move} onPointerUp=${grip?.up} onPointerCancel=${grip?.up}>
        <button class="icon-button" type="button" aria-label="Back to the chat" onClick=${onClose}><${Icon} name="back" /></button>
        <div class="bar-title">
          <span class="bar-label">Project page</span>
          ${page?.unix_ms && html`<span class="bar-state">Updated ${ago(now - page.unix_ms)}</span>`}
        </div>
      </header>
      <div class="scroll project-page">
        <${ProjectFacts} project=${project} overview=${overview} />
        ${failure ? html`<p class="error empty" role="alert">${failure}</p>`
          : !page ? html`<${Loading} />`
          : page.text == null ? html`<p class="empty">No page yet. The manager writes it once it starts.</p>`
          : html`<div class="task-body"><${Markdown} text=${page.text} words=${withChips(project)} prose /></div>`}
      </div>
    </div>`;
}
