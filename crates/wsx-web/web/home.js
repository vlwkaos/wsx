// Home with task_view: one row per granted project, what needs the owner first
// (docs/web-client.md "Home"). A row opens the project's manager chat.
import { ago, html, useNow } from "./lib.js";
import { isAgent, listedSessions, stateOf, useStore } from "./store.js";
import { needTone, needWords } from "./tasks.js";

// The order of need kinds, most urgent first, as the server ranks them.
const URGENCY = { blocked: 0, conflict: 1, failed: 1, approval: 2, brief: 2, verify: 3, land: 4, review: 4 };

// Projects with needs first, by their most urgent kind and then their oldest need of that
// kind; then the rest by their last activity (tasks, manager or agent sessions), newest first.
function homeOrder(projects, tasks, sessions) {
  const rows = projects.map((project) => {
    const overview = tasks[project.id];
    const needs = overview?.needs ?? [];
    const rank = needs.length ? Math.min(...needs.map((item) => URGENCY[item.kind] ?? 5)) : null;
    const first = needs.filter((item) => (URGENCY[item.kind] ?? 5) === rank)
      .reduce((oldest, item) => (oldest && oldest.unix_ms <= item.unix_ms ? oldest : item), null);
    const agents = sessions.filter((session) => session.project.id === project.id);
    const last = Math.max(overview?.last_unix_ms ?? 0, ...agents.map((session) => session.state_since_unix_ms ?? 0)) || null;
    return { project, overview, needs, rank, first, agents, last };
  });
  return rows.sort((left, right) => {
    if ((left.rank === null) !== (right.rank === null)) return left.rank === null ? 1 : -1;
    if (left.rank !== null) return left.rank - right.rank || left.first.unix_ms - right.first.unix_ms;
    return (right.last ?? 0) - (left.last ?? 0);
  });
}

// The project's agent sessions in words, most urgent group first: "claude in main exited",
// "2 working", "claude in main done 5m ago". Null without one.
const SESSION_GROUPS = ["blocked", "error", "exited", "working", "done", "idle", "unknown"];
// Session states that need the owner: their words take the state's tone.
const SESSION_ATTENTION = ["blocked", "error", "exited"];
function sessionWords(agents, now) {
  for (const key of SESSION_GROUPS) {
    const group = agents.filter((session) => stateOf(session).key === key)
      .sort((left, right) => (right.state_since_unix_ms ?? 0) - (left.state_since_unix_ms ?? 0));
    if (!group.length) continue;
    const { word, tone } = stateOf(group[0]);
    const done = key === "done" && group[0].state_since_unix_ms ? ago(now - group[0].state_since_unix_ms) : null;
    const since = done ? ` ${done === "now" ? "just now" : done}` : "";
    const who = group.length === 1 ? `${group[0].agent.provider} in ${group[0].branch}` : `${group.length}`;
    return { text: `${who} ${word}${since}`, tone: SESSION_ATTENTION.includes(key) ? tone : null, key };
  }
  return null;
}

// The row's one status line: the most urgent need, else a session that needs attention or
// works, else the manager's last reply, else the other sessions, else whether there is any task.
function statusLine({ overview, first, agents }, now) {
  if (first) return { text: needWords(first), tone: needTone(first) };
  const sessions = sessionWords(agents, now);
  if (sessions && (SESSION_ATTENTION.includes(sessions.key) || sessions.key === "working")) return sessions;
  // ^ What only the owner can check waits for them, before a release (docs/tasks.md "To check").
  if (overview?.to_check) return { text: `${overview.to_check} to check` };
  if (overview?.last_reply) return { text: overview.last_reply };
  if (sessions) return sessions;
  // ^ Until its tasks are read, a project's row claims nothing about them.
  if (!overview) return { text: "Loading…" };
  return { text: overview.tasks.length ? "Nothing needs you" : "No tasks yet" };
}

export function Home({ rail }) {
  const store = useStore();
  const now = useNow(true);
  const [, , open] = location.hash.split("/");
  let body;
  if (!store.daemon) body = html`<p class="empty">wsx is not running on the host.</p>`;
  else if (!store.loaded) body = html`<p class="empty">Loading…</p>`;
  else if (!store.projects.length) body = html`<p class="empty">wsx manages no project that this site may reach.</p>`;
  else {
    body = html`
      <ul class="rows home-rows" aria-label="Projects">
        ${homeOrder(store.projects, store.tasks, listedSessions(store.sessions).filter(isAgent)).map((row) => {
          const status = statusLine(row, now);
          const count = row.needs.length;
          const working = row.overview?.working ?? 0;
          const last = row.last;
          return html`
            <li class="row" key=${row.project.id}>
              <a class="row-link home-row" href=${`#/p/${row.project.id}`} aria-current=${rail && Number(open) === row.project.id ? "page" : null}>
                <span class="row-main">
                  <span class="row-work">
                    <span class="row-label">${row.project.name}</span>
                    ${count ? html`<span class="count" data-tone="attention" aria-label=${`${count} ${count === 1 ? "needs" : "need"} you`}>${count}</span>` : null}
                  </span>
                  <span class="row-context" data-tone=${status.tone ?? null}>${status.text}</span>
                </span>
                <span class="row-state">
                  ${working ? html`<span>${working} working</span>` : null}
                  ${last ? html`<span class="time">${ago(now - last)}</span>` : null}
                </span>
              </a>
            </li>`;
        })}
      </ul>`;
  }
  return html`
    <div class="page">
      ${!rail && html`<h1 class="title">Home</h1>`}
      <div class="scroll">
        ${body}
      </div>
    </div>`;
}
