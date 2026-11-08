// The project overview: the deploy, the release and the main checkout in words, then one markdown
// file with a mermaid graph that the manager keeps up to date with the Edit tool, shown in the
// sheet over the chat (docs/tasks.md "Project overview", docs/web-client-tasks.md "Project overview").
import { ago, html, useNow } from "./lib.js";
import { useStore } from "./store.js";
import { SheetPage, useSheetRead } from "./ui.js";
import { Markdown } from "./markdown.js";
import { withChips } from "./task-words.js";
import { ProjectFacts } from "./story.js";

export function ProjectOverview({ project, onClose, grip }) {
  const tasks = useStore().tasks[project];
  const [page, failure] = useSheetRead(`api/tasks/${project}/page`, { on: "page", wanted: (detail) => detail.project == null || detail.project === project });
  const now = useNow(Boolean(page?.unix_ms));
  return html`
    <${SheetPage} grip=${grip} onClose=${onClose} className="project-overview" loaded=${Boolean(page)} failure=${failure}
      head=${html`<${ProjectFacts} project=${project} overview=${tasks} />`}
      bar=${html`<span class="bar-label">Overview</span>
        ${page?.unix_ms && html`<span class="bar-state">Updated ${ago(now - page.unix_ms)}</span>`}`}>
      ${page?.text == null ? html`<p class="empty">No overview yet. The manager writes it once it starts.</p>`
        : html`<div class="task-body"><${Markdown} text=${page.text} words=${withChips(project)} prose /></div>
          ${page.cut && html`<p class="empty over-limit">Over the size limit: ${page.cut.lines} lines, ${page.cut.chars} characters. Only the first 60 lines and 4,000 characters show.</p>`}`}
    </${SheetPage}>`;
}
