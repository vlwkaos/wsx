// Files: browse one granted project; edit text when the site allows it.
import { api } from "./api.js";
import { html, useEffect, useState } from "./lib.js";
import { can, useStore } from "./store.js";
import { Back, toast } from "./ui.js";

function Editor({ project, path, text }) {
  const [original, setOriginal] = useState(text);
  const [value, setValue] = useState(text);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  if (!can("files_write")) return html`<pre class="code" tabindex="0">${text}</pre>`;
  const save = async () => {
    setBusy(true);
    try {
      await api("api/file", { method: "PUT", json: { project, path, original, content: value } });
      setOriginal(value);
      setError("");
      toast(`Saved ${path}.`);
    } catch (failure) {
      // The edited text stays in the editor so nothing typed is lost.
      setError(failure.message);
    }
    setBusy(false);
  };
  return html`
    <textarea class="code" aria-label=${path} spellcheck=${false} value=${value} onInput=${(event) => setValue(event.target.value)}></textarea>
    <div class="actions">
      <button class="button button-primary" type="button" disabled=${busy} onClick=${save}>Save</button>
      ${error && html`<span class="error" role="alert">${error}</span>`}
    </div>`;
}

export function Files({ project }) {
  const store = useStore();
  const [path, setPath] = useState("");
  const [view, setView] = useState(null);
  const name = store.projects.find((candidate) => candidate.id === project)?.name ?? "Project";

  const open = async (target, file) => {
    setPath(target);
    setView(null);
    try {
      const data = file
        ? await api("api/file", { query: { project, path: target } })
        : await api("api/files", { query: { project, path: target } });
      setView({ file, ...data });
    } catch (failure) {
      setView({ error: failure.message });
    }
  };
  useEffect(() => { open("", false); }, [project]);

  const parts = path ? path.split("/") : [];
  let body = html`<p class="empty">Loading…</p>`;
  if (view?.error) body = html`<p class="error" role="alert">${view.error}</p>`;
  else if (view?.file) {
    body = view.binary ? html`<p class="empty">This file is not text and cannot be shown here.</p>`
      : html`<${Editor} key=${path} project=${project} path=${path} text=${view.text} />`;
  } else if (view) {
    body = html`
      ${view.entries.length ? html`
        <ul class="rows">
          ${view.entries.map((entry) => {
            const target = path ? `${path}/${entry.name}` : entry.name;
            return html`
              <li class="row" key=${entry.name}>
                <button class="row-link" type="button" onClick=${() => open(target, !entry.directory)}>
                  <span class="row-main"><span class="row-label path">${entry.directory ? `${entry.name}/` : entry.name}</span></span>
                </button>
              </li>`;
          })}
        </ul>` : html`<p class="empty">This folder is empty.</p>`}
      ${view.truncated && html`<p class="note">Only the first 512 entries are shown.</p>`}`;
  }
  return html`
    <div class="page files">
      <header class="bar">
        <${Back} fallback=${`#/p/${project}`} />
        <nav class="crumbs" aria-label="Folder path">
          <button class="link-action" type="button" onClick=${() => open("", false)}>${name}</button>
          ${parts.map((part, index) => {
            const last = index === parts.length - 1;
            return html`<span class="crumb-mark" aria-hidden="true">/</span>
              ${last && view?.file ? html`<strong class="path">${part}</strong>`
                : html`<button class="link-action path" type="button" onClick=${() => open(parts.slice(0, index + 1).join("/"), false)}>${part}</button>`}`;
          })}
        </nav>
      </header>
      <div class="scroll file-body">${body}</div>
    </div>`;
}
