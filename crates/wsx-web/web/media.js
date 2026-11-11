// The project's media archive: the To check captures, the pictures and videos that messages link
// and the files the owner attached, newest first, from `api/tasks/<project>/media`
// (docs/web-client-tasks.md "Media"). A tile opens the image viewer (viewer.js) with the others.
import { api, resolve } from "./api.js";
import { ago, bytes, html, Loading, useEffect, useState } from "./lib.js";
import { useSheetRead, SheetPage } from "./ui.js";
import { TaskChip } from "./task-words.js";
import { Capture } from "./viewer.js";

const KINDS = [["", "All"], ["image", "Images"], ["video", "Videos"]];
const VERDICTS = { passed: "Passed", failed: "Failed", skipped: "Skipped" };

// What a tile says about its file: a capture's verdict, where a file came from, and why it is
// not shown (a capture the budget removed, a file off the checkout or an upload past its TTL).
function caption(item) {
  const gone = item.gone ? (item.source === "upload" ? "Expired" : "Removed") : !item.url ? "Unavailable" : "";
  const from = item.source === "capture" ? VERDICTS[item.verdict] ?? "To check" : item.source === "upload" ? "Upload" : item.who;
  return [gone, from, ago(Date.now() - item.unix_ms)].filter(Boolean).join(" · ");
}

function Tile({ project, item }) {
  const video = item.kind === "video";
  const name = item.text.split("/").pop();
  return html`
    <li class="media-tile" data-gone=${item.url ? null : ""}>
      ${item.url
        ? html`<${Capture} href=${resolve(item.url)} download=${item.source === "capture" ? `${resolve(item.url)}?download=1` : undefined}
            video=${video} poster=${item.poster_url && resolve(item.poster_url)} label=${`${video ? "Video" : "Image"}: ${name}`} />`
        : html`<span class="media-gap" aria-hidden="true"></span>`}
      <p class="media-line">
        ${item.task != null && html`<${TaskChip} project=${project} id=${item.task} />`}
        <span class="media-name">${name}</span>
      </p>
      <p class="media-facts">${caption(item)}${item.size ? ` · ${bytes(item.size)}` : ""}</p>
    </li>`;
}

// One filtered list: the first page, then the pages "More" brings. Keyed by its filters, so a
// filter starts a new list.
function Results({ project, query, onTasks }) {
  const path = `api/tasks/${project}/media${query}`;
  const [first, failure] = useSheetRead(path);
  const [more, setMore] = useState({ pages: [], busy: false, failure: "" });
  const pages = first ? [first, ...more.pages] : [];
  const items = pages.flatMap((page) => page.items);
  const next = pages.at(-1)?.next;
  useEffect(() => { if (first) onTasks(first.items.map((item) => item.task).filter((task) => task != null)); }, [first]);
  const load = async () => {
    setMore({ ...more, busy: true, failure: "" });
    try {
      const page = await api(path, { query: { before: next } });
      setMore({ pages: [...more.pages, page], busy: false, failure: "" });
    } catch (error) {
      setMore({ ...more, busy: false, failure: error.message });
    }
  };
  if (failure) return html`<p class="error empty" role="alert">${failure}</p>`;
  if (!first) return html`<${Loading} />`;
  if (!items.length) return html`<p class="empty">No media</p>`;
  return html`
    <ul class="media-grid" data-gallery aria-label="Media">${items.map((item) => html`<${Tile} key=${item.id} project=${project} item=${item} />`)}</ul>
    ${more.failure && html`<p class="error" role="alert">${more.failure}</p>`}
    ${next && html`<button class="button media-more" type="button" disabled=${more.busy} onClick=${load}>${more.busy ? "Loading…" : "More"}</button>`}`;
}

function MediaView({ project }) {
  const [filter, setFilter] = useState({ kind: "", task: "" });
  const [tasks, setTasks] = useState([]);
  const query = `?${new URLSearchParams(Object.entries(filter).filter(([, value]) => value))}`;
  const seen = (found) => setTasks((known) => [...new Set([...known, ...found])].sort((a, b) => b - a));
  return html`
    <div class="media-filters">
      <div class="media-kinds" role="group" aria-label="Kind">
        ${KINDS.map(([value, name]) => html`<button key=${value} type="button" class="media-chip" aria-pressed=${filter.kind === value} onClick=${() => setFilter({ ...filter, kind: value })}>${name}</button>`)}
      </div>
      ${tasks.length > 0 && html`
        <label class="media-task"><span class="visually-hidden">Task</span>
          <select name="task" value=${filter.task} onChange=${(event) => setFilter({ ...filter, task: event.target.value })}>
            <option value="">All tasks</option>
            ${tasks.map((id) => html`<option key=${id} value=${id}>#${id}</option>`)}
          </select>
        </label>`}
    </div>
    <${Results} key=${query} project=${project} query=${query === "?" ? "" : query} onTasks=${seen} />`;
}

export const MediaPage = ({ project, onClose, grip }) => html`
  <${SheetPage} grip=${grip} onClose=${onClose} bar=${html`<span class="bar-label">Media</span>`} className="media">
    <${MediaView} project=${project} />
  </${SheetPage}>`;
