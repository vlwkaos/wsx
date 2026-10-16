// Home with task_view: one row per granted project, what needs the owner first
// (docs/web-client.md "Home"). A row opens the project's manager chat.
import { ago, html, useNow } from "./lib.js";
import { useStore } from "./store.js";
import { needTone, needWords } from "./tasks.js";

// The order of need kinds, most urgent first, as the server ranks them.
const URGENCY = { blocked: 0, conflict: 1, failed: 1, approval: 2, verify: 3, land: 4, review: 4 };

// Projects with needs first, by their most urgent kind and then their oldest need of that
// kind; then the rest by their last activity, newest first.
export function homeOrder(projects, tasks) {
  const rows = projects.map((project) => {
    const overview = tasks[project.id];
    const needs = overview?.needs ?? [];
    const rank = needs.length ? Math.min(...needs.map((item) => URGENCY[item.kind] ?? 5)) : null;
    const first = needs.filter((item) => (URGENCY[item.kind] ?? 5) === rank)
      .reduce((oldest, item) => (oldest && oldest.unix_ms <= item.unix_ms ? oldest : item), null);
    return { project, overview, needs, rank, first };
  });
  return rows.sort((left, right) => {
    if ((left.rank === null) !== (right.rank === null)) return left.rank === null ? 1 : -1;
    if (left.rank !== null) return left.rank - right.rank || left.first.unix_ms - right.first.unix_ms;
    return (right.overview?.last_unix_ms ?? 0) - (left.overview?.last_unix_ms ?? 0);
  });
}

// The row's one status line: the most urgent need, else the manager's last reply, else whether
// there is any task at all.
function statusLine({ overview, first }) {
  if (first) return { text: needWords(first), tone: needTone(first) };
  if (overview?.last_reply) return { text: overview.last_reply };
  return { text: overview?.tasks?.length ? "Nothing needs you" : "No tasks yet" };
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
        ${homeOrder(store.projects, store.tasks).map((row) => {
          const status = statusLine(row);
          const count = row.needs.length;
          const working = row.overview?.working ?? 0;
          const last = row.overview?.last_unix_ms;
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
        <p class="home-more"><a class="link-action" href="#/sessions">All sessions</a></p>
      </div>
    </div>`;
}
