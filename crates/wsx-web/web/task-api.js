// Reading the task overviews and running a fixed action on a task or an open item, shared by the
// manager chat, the sheets and main.js (docs/web-client-tasks.md "Project screen").
import { api } from "./api.js";
import { state, update } from "./store.js";
import { toast } from "./ui.js";

// Every granted project's overview, or one project's. `missing` reads one project only when
// the read of every project (at the stream's open) ended without it.
let readingAll = null;
export async function loadTasks(project, { missing = false } = {}) {
  try {
    if (project == null) {
      readingAll = api("api/tasks");
      const all = await readingAll;
      all.forEach(noteDecisions);
      update({ tasks: Object.fromEntries(all.map((overview) => [overview.project, overview])) });
    } else {
      if (missing) {
        if (!readingAll) return;
        await readingAll.catch(() => {});
        if (state.tasks[project]) return;
      }
      const overview = await api(`api/tasks/${project}`);
      noteDecisions(overview);
      update({ tasks: { ...state.tasks, [project]: overview } });
    }
  } catch { /* the next tasks event brings it */ }
}

// Actions whose outcome the chat already shows ("Approved land #3", the worker's "#3 · done").
const SAID_IN_CHAT = new Set(["approve", "deny"]);

// What a fixed action did, as a notice: wsx's own answer, unless the chat says it already.
function announce(name, action, answer) {
  if (SAID_IN_CHAT.has(action) || (action === "land" && answer.state === "landed")) return;
  toast(`${name}: ${answer.message || "done"}`);
}

// An approved action that did not run is a failure the owner acts on: it stays until
// dismissed. Decisions seen before this page loaded say nothing, and neither does one the
// owner gave in the chat: the manager runs that action itself and replies with its result.
const seenDecisions = new Map();
export function noteDecisions(overview) {
  const known = seenDecisions.get(overview.project);
  seenDecisions.set(overview.project, new Set((overview.decisions ?? []).map((decision) => decision.approval)));
  if (!known) return;
  for (const decision of overview.decisions ?? []) {
    if (known.has(decision.approval) || !decision.approved || decision.owner_said != null
      || decision.outcome?.startsWith("done")) continue;
    const what = [decision.action, decision.task != null && `#${decision.task}`].filter(Boolean).join(" ");
    toast(`${what.charAt(0).toUpperCase()}${what.slice(1)} did not run: ${decision.outcome}`, { failure: true });
  }
}

// One fixed action. A success says what wsx did.
export async function act(project, item, action) {
  const path = item.kind === "approval"
    ? `api/tasks/${project}/approvals/${item.approval}/${action}`
    : item.kind === "ask" ? `api/tasks/${project}/asks/${item.id}/${action}`
    : `api/tasks/${project}/t/${item.task}/${action}`;
  const answer = await api(path, { method: "POST", json: {} });
  const name = item.kind === "approval" || item.kind === "ask" ? item.label : `#${item.task}`;
  announce(name, action, answer);
  await loadTasks(project);
  return answer;
}
