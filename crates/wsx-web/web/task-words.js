// The words and marks of tasks, shared by the manager chat, the sheets and the lists: the stage
// and status of a task in words and tone, the `#n` chip, and `withChips` (docs/web-client-tasks.md).
// ^ Imports nothing from tasks.js, so the screens that show a task need not import the manager.
import { html, useEffect, useRef, useState } from "./lib.js";
import { state, useStore } from "./store.js";

// Motion only where the user allows it (the no-preference block of app.css); under reduce,
// nothing moves and nothing waits for an animation to end.
export const calm = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
export const LEAVE_MS = 200;

// True for `ms` after `flag` stops holding, so a thing can play its exit.
function useAfter(flag, ms) {
  const [after, setAfter] = useState(false);
  const was = useRef(flag);
  useEffect(() => {
    if (was.current && !flag && !calm()) {
      setAfter(true);
      setTimeout(() => setAfter(false), ms);
    }
    was.current = flag;
  }, [flag]);
  return after;
}

// The three states the owner sees, and the tone that repeats each (docs/web-client-tasks.md);
// the server maps the store's states to them. Working has its own color.
const STAGES = {
  working: ["working", "working"], needs_you: ["needs you", "attention"], done: ["done", "muted"], closed: ["closed", "muted"],
};
export const taskStage = (task) => STAGES[task?.stage] ?? [task?.stage ?? "", "muted"];
export const taskLink = (project, task) => `#/p/${project}/t/${task}`;

// A task the agents work on: its chip and its reservoir row pulse.
export const running = (task) => task?.stage === "working";

// `#12` in any text: the number and a state dot, the state in its accessible name and the
// title on hover. The title shows in the sheet and the reservoir, not in the text. Another
// project's task (`name`) shows its project too: "pizzahouse #34".
export function TaskChip({ project, id, name = "", plain = false }) {
  const store = useStore();
  const task = store.tasks[project]?.tasks.find((candidate) => candidate.id === id);
  const label = `${name}${name && " "}#${id}`;
  // ^ `plain`: a span, for a row whose whole text is the link (anchors cannot nest).
  if (plain) {
    return html`<span class="task-chip">${task && html`<${StateMark} task=${task} />`}${label}${task && html`<span class="visually-hidden">, ${taskStage(task)[0]}</span>`}</span>`;
  }
  if (!task) return html`<a class="task-chip" href=${taskLink(project, id)}>${label}</a>`;
  return html`
    <a class="task-chip" href=${taskLink(project, id)} title=${task.title} aria-label=${`${label}, ${taskStage(task)[0]}`}>
      <${StateMark} task=${task} />${label}
    </a>`;
}

// A ready task or a plan waits for the manager's review: it is under Running, but nothing runs.
const awaitsManager = (task) => task.state === "ready" || task.waiting?.on === "plan";

// A task's state dot. A task that stops running settles its dot once instead of snapping from the pulse.
function StateMark({ task }) {
  const live = running(task) && !awaitsManager(task);
  const settle = useAfter(live, LEAVE_MS * 3);
  return html`<span class="mark" data-tone=${taskStage(task)[1]} data-running=${live ? "" : null} data-settle=${settle ? "" : null} aria-hidden="true"></span>`;
}

// A task in a list: its title first, then its number, small and muted, never cut. `state` is
// for a screen reader, where the row's dot shows it.
export const TaskTitle = ({ title, id, state }) => html`
  <span class="row-title"><span class="row-label">${title}</span><span class="task-number">#${id}</span>${state && html`<span class="visually-hidden">, ${state}</span>`}</span>`;

// One text-to-nodes function per project and sender, so the markdown of a message is parsed
// once: `#n` becomes a chip, "<project> #n" a chip of that granted project's task, and "project
// overview" a link to it. `source` is the project whose manager wrote the text: its bare `#n`
// is that project's task, and plain text when the site does not grant it
// (docs/web-client-tasks.md "Project screen").
const chipWords = new Map();
const escaped = (name) => name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
export function withChips(project, source = null) {
  const named = state.projects.filter((other) => other.name).sort((left, right) => right.name.length - left.name.length);
  const key = `${project}/${source}/${named.map((other) => `${other.id}:${other.name}`)}`;
  if (!chipWords.has(key)) {
    // ^ Whose a bare `#n` is: this project's, the sender's, or none (a sender the site does not grant).
    const bareOwner = source == null ? project : named.find((other) => other.name === source)?.id;
    const find = new RegExp(`${named.length ? `(${named.map((other) => escaped(other.name)).join("|")})[ \\t]+#(\\d+)\\b` : "(?!)()()"}|#(\\d+)\\b|\\bproject overview\\b`, "gi");
    const chip = (owner, id) => html`<${TaskChip} project=${owner} id=${Number(id)} name=${owner === project ? "" : named.find((other) => other.id === owner).name} />`;
    chipWords.set(key, (text) => {
      const nodes = [];
      let from = 0;
      find.lastIndex = 0;
      for (let found; (found = find.exec(text));) {
        const [word, name, mentioned, bare] = found;
        // ^ A name inside a longer word ("unpizzahouse #3") names nothing: the `#n` after it is bare.
        if (name && /[\w-]/.test(text[found.index - 1] ?? "")) {
          find.lastIndex = found.index + name.length;
          continue;
        }
        nodes.push(text.slice(from, found.index));
        from = found.index + word.length;
        if (mentioned) {
          const owner = named.find((other) => other.name.toLowerCase() === name.toLowerCase()).id;
          if (owner === project) nodes.push(word.slice(0, word.indexOf("#")));
          nodes.push(chip(owner, mentioned));
        } else if (bare) {
          nodes.push(bareOwner == null ? word : chip(bareOwner, bare));
        } else {
          nodes.push(html`<a class="page-link" href=${`#/p/${project}/page`}>${word}</a>`);
        }
      }
      nodes.push(text.slice(from));
      return nodes.filter((node) => node !== "");
    });
  }
  return chipWords.get(key);
}

export const KIND_TONE = { waiting: "attention", failed: "danger", approval: "attention", land: "accent", ask: "accent" };
// One open item in words, as the owner would read it: "#4 Waiting for your answer", "Approve
// land #3". The words are the server's `status` (wsx-core task::status, docs/tasks.md "Status").
export const needWords = (item) => item.kind === "approval" || item.kind === "ask" ? item.label : `#${item.task} ${item.status.label}`;
export const needTone = (item) => KIND_TONE[item.kind];

// The tone of a task's status by its `status.kind` (docs/tasks.md "Status"), else its stage's;
// its words are the server's `status` (docs/web-client-tasks.md "Task sheet").
const STATUS_TONE = { approval: "attention", owner: "attention", stopped: "attention", interrupted: "attention", failed: "danger", stuck: "danger" };
export const statusTone = (task) => STATUS_TONE[task.status?.kind] ?? taskStage(task)[1];

// The agent in use now, in few words: "sonnet high", "pi/qwen high", "fallback pi/qwen": the
// effort wsx passes follows the model (docs/web-client-tasks.md "Agent"; view.rs `agent_label`).
export const agentWords = (agent) => !agent?.model ? ""
  : `${agent.fallback ? "fallback " : ""}${agent.adapter === "claude" && !agent.fallback ? "" : `${agent.adapter}/`}${agent.model}${agent.effort ? ` ${agent.effort}` : ""}`;
