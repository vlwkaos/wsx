// Adopted: a project's worktrees that no task owns, with their agent sessions, read only
// (docs/web-client.md "Project screen"). A session row opens the session; nothing here
// starts, renames, moves or closes a worktree or a session.
import { ago, html, Icon, useNow, useState } from "./lib.js";
import { isAgent, listedSessions, sessionLink, stateOf, useStore } from "./store.js";

// The fold of each project's section on this page; open until the reader folds it.
const folded = new Set();

// The worktrees without a task, main checkout first, each with its agent sessions, newest
// state first. The main checkout counts only with an agent session. Null until the
// project's task branches are read: the list claims nothing before.
export function adoptedOf(project, overview, sessions) {
  const branches = overview?.task_branches;
  if (!branches) return null;
  const agents = listedSessions(sessions).filter((session) => isAgent(session) && session.project.id === project.id);
  return [...project.worktrees]
    .sort((left, right) => Number(right.main) - Number(left.main))
    .filter((worktree) => !worktree.branch || !branches.includes(worktree.branch))
    .map((worktree) => ({
      worktree,
      sessions: agents.filter((session) => session.worktree === worktree.id)
        .sort((left, right) => (right.state_since_unix_ms ?? 0) - (left.state_since_unix_ms ?? 0)),
    }))
    .filter((item) => !item.worktree.main || item.sessions.length);
}

const counted = (count) => (count === 1 ? "1 worktree" : `${count} worktrees`);

export function Adopted({ project, strip }) {
  const store = useStore();
  const now = useNow(true);
  const [open, setOpen] = useState(!folded.has(project.id));
  const items = adoptedOf(project, store.tasks[project.id], store.sessions);
  if (!items?.length) return null;
  const toggle = (event) => {
    const opened = event.currentTarget.open;
    if (opened) folded.delete(project.id);
    else folded.add(project.id);
    setOpen(opened);
  };
  const rows = items.flatMap(({ worktree, sessions }) => {
    const branch = worktree.branch || "detached";
    if (!sessions.length) {
      return [html`
        <li class="row" key=${`w${worktree.id}`}>
          <div class="row-link">
            <span class="mark" aria-hidden="true"></span>
            <span class="row-main">
              <span class="row-work"><span class="row-worktree">${branch}</span></span>
              <span class="row-context">No agent session</span>
            </span>
          </div>
        </li>`];
    }
    return sessions.map((session) => {
      const status = stateOf(session);
      const since = session.state_since_unix_ms;
      return html`
        <li class="row" key=${`s${session.id}`}>
          <a class="row-link" href=${sessionLink(session)}>
            <span class="mark" data-tone=${status.tone} aria-hidden="true"></span>
            <span class="row-main">
              <span class="row-work"><span class="row-worktree">${branch}</span></span>
              <span class="row-context">${session.label} · ${session.agent.provider}</span>
            </span>
            <span class="row-state" data-tone=${status.tone}>
              <span>${status.word}</span>
              ${since != null && html`<span class="time">${ago(now - since)}</span>`}
            </span>
          </a>
        </li>`;
    });
  });
  return html`
    <details class=${strip ? "adopted adopted-strip" : "adopted"} open=${open} onToggle=${toggle} aria-label="Adopted">
      <summary class="adopted-line"><${Icon} name="chevron" /><span class="adopted-text">Adopted · ${counted(items.length)} without a task</span></summary>
      <ul class="rows">${rows}</ul>
    </details>`;
}
