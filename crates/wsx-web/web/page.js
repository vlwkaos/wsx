// The project page: the deploy, the release and the main checkout in words, then one markdown
// file with a mermaid graph that the manager keeps up to date with the Edit tool, shown in the
// sheet over the chat (docs/tasks.md "Project page", docs/web-client-tasks.md "Project page").
import { api } from "./api.js";
import { ago, html, Loading, useCoalescedLoad, useNow, useState } from "./lib.js";
import { useEventLoad, useStore } from "./store.js";
import { SheetBar } from "./ui.js";
import { Markdown } from "./markdown.js";
import { withChips } from "./tasks.js";
import { ProjectFacts } from "./story.js";

export function ProjectPage({ project, onClose, grip }) {
  const overview = useStore().tasks[project];
  const [page, setPage] = useState(null);
  const [failure, setFailure] = useState("");
  const now = useNow(Boolean(page?.unix_ms));
  const read = useCoalescedLoad(async (current) => {
    const value = await api(`api/tasks/${project}/page`);
    if (!current()) return;
    setPage(value);
    setFailure("");
  }, (error) => setFailure(error.message), [project]);
  useEventLoad("page", read, (detail) => detail.project == null || detail.project === project, [project]);
  return html`
    <div class="page task-page">
      <${SheetBar} grip=${grip} onClose=${onClose}>
        <span class="bar-label">Project page</span>
        ${page?.unix_ms && html`<span class="bar-state">Updated ${ago(now - page.unix_ms)}</span>`}
      </${SheetBar}>
      <div class="scroll project-page">
        <${ProjectFacts} project=${project} overview=${overview} />
        ${failure ? html`<p class="error empty" role="alert">${failure}</p>`
          : !page ? html`<${Loading} />`
          : page.text == null ? html`<p class="empty">No page yet. The manager writes it once it starts.</p>`
          : html`<div class="task-body"><${Markdown} text=${page.text} words=${withChips(project)} prose /></div>`}
      </div>
    </div>`;
}
