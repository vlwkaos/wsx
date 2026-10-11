// Files panel: browse the one granted project; edit text when the site allows it.
import { api } from "./api.js";
import { toast } from "./ui.js";
import { add, fill, h } from "./dom.js";

export function filesView(app, main) {
  let closed = false;
  const crumbs = h("nav", { class: "row", "aria-label": "Folder path" });
  const body = h("div", { class: "stack" });
  add(main, h("h2", {}, "Files"), crumbs, body);

  function renderCrumbs(path, isFile) {
    const parts = path ? path.split("/") : [];
    const items = [h("button", { type: "button", onclick: () => list("") }, "Project")];
    parts.forEach((part, index) => {
      const target = parts.slice(0, index + 1).join("/");
      const last = index === parts.length - 1;
      items.push(
        h("span", { class: "muted", "aria-hidden": "true" }, "/"),
        last && isFile ? h("strong", {}, part) : h("button", { type: "button", onclick: () => list(target) }, part),
      );
    });
    fill(crumbs, ...items);
  }

  async function list(path) {
    renderCrumbs(path, false);
    try {
      const data = await api("api/files", { query: { path } });
      if (closed) return;
      const rows = data.entries.map((entry) => {
        const target = path ? `${path}/${entry.name}` : entry.name;
        return h("li", { class: "list-row" }, h("button", { type: "button", onclick: () => (entry.directory ? list(target) : open(target)) },
          entry.directory ? `${entry.name}/` : entry.name));
      });
      fill(body,
        rows.length ? h("ul", { class: "list" }, rows) : h("p", { class: "muted" }, "This folder is empty."),
        data.truncated ? h("p", { class: "muted small" }, "Only the first 512 entries are shown.") : null,
      );
    } catch (failure) {
      fill(body, h("p", { class: "error", role: "alert" }, failure.message));
    }
  }

  async function open(path) {
    renderCrumbs(path, true);
    try {
      const data = await api("api/file", { query: { path } });
      if (closed) return;
      if (data.binary) {
        fill(body, h("p", { class: "muted" }, "This file is not text and cannot be shown here."));
        return;
      }
      if (!app.can("files_write")) {
        fill(body, h("pre", { class: "terminal-frame", tabIndex: 0 }, data.text));
        return;
      }
      let original = data.text;
      const message = h("p", { class: "small error", role: "alert" });
      const editor = h("textarea", { class: "terminal-frame", "aria-label": path, spellcheck: false, value: data.text });
      const save = h("button", {
        class: "button-primary", type: "button",
        onclick: async () => {
          save.disabled = true;
          try {
            await api("api/file", { method: "PUT", json: { path, original, content: editor.value } });
            original = editor.value;
            message.textContent = "";
            toast(`Saved ${path}.`);
          } catch (failure) {
            // The edited text stays in the editor so nothing typed is lost.
            message.textContent = failure.message;
          }
          save.disabled = false;
        },
      }, "Save");
      fill(body, editor, h("div", { class: "row" }, save, message));
    } catch (failure) {
      fill(body, h("p", { class: "error", role: "alert" }, failure.message));
    }
  }

  list("");
  return { close: () => { closed = true; } };
}
