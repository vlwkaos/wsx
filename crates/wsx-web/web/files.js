// Files: browse one granted project; edit text when the site allows it.
import { api } from "./api.js";
import { html, Loading, useEffect, useState } from "./lib.js";
import { Markdown } from "./markdown.js";
import { can, useStore } from "./store.js";
import { Back, toast } from "./ui.js";

function Editor({ project, path, text, onSaved }) {
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
      onSaved?.(value);
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

const isMarkdown = (path) => /\.(md|markdown)$/i.test(path);
const isPage = (path) => /\.html?$/i.test(path);

// A markdown or HTML file opens rendered; Source shows the text and the editor, which stays
// mounted so a draft survives the switch (docs/web-client.md "Files"). An HTML file is a page
// in a frame with no script and an opaque origin, so it never reaches the site or its API.
function FileView({ project, path, text, pageToken, sourceError, source }) {
  const [saved, setSaved] = useState(text);
  const name = path.split("/").pop();
  return html`
    ${!source && (pageToken
      ? html`<iframe class="file-page" src=${`view/${pageToken}/${encodeURIComponent(name)}`} title=${name}
          sandbox="allow-popups allow-popups-to-escape-sandbox" referrerpolicy="no-referrer"></iframe>`
      : html`<div class="file-markdown"><${Markdown} text=${saved} /></div>`)}
    <div class="file-source" hidden=${!source}>${sourceError ? html`<p class="error" role="alert">${sourceError}</p>`
      : html`<${Editor} project=${project} path=${path} text=${text} onSaved=${setSaved} />`}</div>`;
}

export function Files({ project, file: linked }) {
  const store = useStore();
  const [path, setPath] = useState("");
  const [view, setView] = useState(null);
  const [rendered, setRendered] = useState(true);
  const name = store.projects.find((candidate) => candidate.id === project)?.name ?? "Project";

  const open = async (target, file) => {
    setPath(target);
    setView(null);
    setRendered(true);
    try {
      const query = { project, path: target };
      const read = api(file ? "api/file" : "api/files", { query });
      // ^ A page over the 1 MiB text limit still shows; only its Source says why it cannot.
      const asPage = file && isPage(target);
      const [data, token] = await Promise.all([
        asPage ? read.catch((failure) => ({ sourceError: failure.message })) : read,
        asPage ? api("api/file/page", { query }).then((answer) => answer.token) : null,
      ]);
      setView({ file, pageToken: token, ...data });
    } catch (failure) {
      setView({ error: failure.message });
    }
  };
  useEffect(() => { open(linked ?? "", Boolean(linked)); }, [project]);

  const parts = path ? path.split("/") : [];
  let body = html`<${Loading} />`;
  const rendering = Boolean(view?.file && !view.binary && (isMarkdown(path) || view.pageToken));
  if (view?.error) body = html`<p class="error" role="alert">${view.error}</p>`;
  else if (view?.file) {
    body = view.binary ? html`<p class="empty">This file is not text and cannot be shown here.</p>`
      : html`<${FileView} key=${path} project=${project} path=${path} text=${view.text} pageToken=${view.pageToken} sourceError=${view.sourceError} source=${!(rendering && rendered)} />`;
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
        ${rendering && html`<button class="link-action" type="button" onClick=${() => setRendered(!rendered)}>${rendered ? "Source" : "Rendered"}</button>`}
      </header>
      <div class="scroll file-body">${body}</div>
    </div>`;
}
