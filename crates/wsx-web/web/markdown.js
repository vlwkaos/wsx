// Markdown in agent messages (docs/web-client.md "Markdown").
// ^ markdown-it only parses: this file builds the page's nodes from its token list.
// No HTML string from a message is ever given to the DOM, raw HTML in a message stays
// text, and a message cannot load a remote resource.
import "./markdown-it.js";
import hljs from "./highlight.js";
import { resolve } from "./api.js";
import { h } from "./preact.js";
import { html, useMemo, useState } from "./lib.js";
import { Diagram } from "./diagram.js";

const parser = globalThis.markdownit({ html: false, linkify: true });
// Only text with a scheme becomes a link: `main.rs` and `notes.io` are file names here.
parser.linkify.set({ fuzzyLink: false, fuzzyEmail: false });

const LINK = /^(https?:|mailto:)/i;
// Fence names people write, as the highlight.js grammar that reads them.
const LANGUAGES = {
  rust: "rust", rs: "rust", typescript: "typescript", ts: "typescript", tsx: "typescript",
  javascript: "javascript", js: "javascript", jsx: "javascript", mjs: "javascript",
  python: "python", py: "python", bash: "bash", sh: "bash", shell: "bash", zsh: "bash", console: "bash",
  json: "json", jsonc: "json", toml: "ini", ini: "ini", yaml: "yaml", yml: "yaml",
  diff: "diff", patch: "diff", html: "xml", xml: "xml", svg: "xml", css: "css", sql: "sql", go: "go", golang: "go",
};
// Larger blocks stay uncolored: the grammar pass is the slow part of a long message.
const MAX_HIGHLIGHT_CHARS = 40_000;

// The highlight.js token tree as nodes. A scope such as `title.function` uses its first word.
const colored = (node) => node.children.map((child) => typeof child === "string" ? child
  : html`<span class=${child.scope ? `tok-${child.scope.split(".")[0]}` : null}>${colored(child)}</span>`);

function Code({ source, language }) {
  const [copied, setCopied] = useState("");
  const grammar = LANGUAGES[language.toLowerCase()];
  const body = useMemo(() => {
    if (!grammar || source.length > MAX_HIGHLIGHT_CHARS) return source;
    try {
      return colored(hljs.highlight(source, { language: grammar, ignoreIllegals: true })._emitter.rootNode);
    } catch {
      return source;
    }
  }, [source, grammar]);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(source);
      setCopied("Copied");
    } catch {
      setCopied("Copy failed");
    }
    setTimeout(() => setCopied(""), 2000);
  };
  return html`
    <div class="code-block">
      <div class="code-head">
        <span class="code-language">${language}</span>
        <button class="link-action" type="button" aria-live="polite" onClick=${copy}>${copied || "Copy"}</button>
      </div>
      <pre tabindex="0"><code>${body}</code></pre>
    </div>`;
}

const ALIGN = /text-align:(left|center|right)/;

function inline(tokens, files, words) {
  const stack = [[]];
  const push = (node) => stack.at(-1).push(node);
  for (const token of tokens ?? []) {
    if (token.nesting === 1) {
      stack.push([]);
    } else if (token.nesting === -1) {
      const children = stack.pop();
      if (token.type === "link_close") {
        const href = token.meta;
        // Any other scheme, and a relative address, stays text.
        push(LINK.test(href) ? html`<a href=${href} target="_blank" rel="noopener noreferrer">${children}</a>` : children);
      } else {
        push(h(token.tag, null, children));
      }
    } else if (token.type === "text") {
      for (const node of words ? words(token.content) : [token.content]) push(node);
    } else if (token.type === "code_inline") {
      push(html`<code>${token.content}</code>`);
    } else if (token.type === "softbreak") {
      push("\n");
    } else if (token.type === "hardbreak") {
      push(html`<br />`);
    } else if (token.type === "image") {
      // Only a file the server already granted for this message is shown as a picture.
      const name = (token.attrGet("src") ?? "").split(/[?#]/)[0].split("/").at(-1);
      const file = files?.find((candidate) => candidate.image && candidate.name === name);
      push(file ? html`<img src=${resolve(file.url)} alt=${token.content} loading="lazy" />`
        : html`<span class="image-alt">${token.content || "image"}</span>`);
    } else {
      push(token.content);
    }
  }
  return stack[0];
}

// A list item that starts with "[ ]" or "[x]" is a task: the mark becomes a checkbox.
function task(children) {
  // The item's first text is in its paragraph, which a tight list leaves as a bare list.
  let lead = children[0];
  if (lead && !Array.isArray(lead)) lead = lead.props?.children;
  const match = Array.isArray(lead) && typeof lead[0] === "string" && /^\[([ xX])\]\s+/.exec(lead[0]);
  if (!match) return null;
  lead[0] = lead[0].slice(match[0].length);
  return match[1] !== " ";
}

function blocks(tokens, files, words) {
  const stack = [{ children: [] }];
  const push = (...nodes) => stack.at(-1).children.push(...nodes);
  for (const token of tokens) {
    if (token.type === "inline") {
      push(...inline(token.children, files, words));
    } else if (token.nesting === 1) {
      // The closing token carries no attributes: keep the opening one.
      stack.push({ open: token, children: [] });
    } else if (token.nesting === -1) {
      const { open, children } = stack.pop();
      const tag = open.tag;
      if (tag === "p") {
        // A tight list hides its paragraphs.
        if (open.hidden) push(children);
        else push(html`<p>${children}</p>`);
      } else if (tag === "li") {
        const done = task(children);
        push(done === null ? html`<li>${children}</li>`
          : html`<li class="task"><input type="checkbox" checked=${done} disabled aria-label=${done ? "Done" : "Not done"} />${children}</li>`);
      } else if (tag === "ol") {
        push(h("ol", { start: open.attrGet("start") ?? undefined }, children));
      } else if (tag === "table") {
        push(html`<div class="table-wrap" tabindex="0"><table>${children}</table></div>`);
      } else if (tag === "th" || tag === "td") {
        push(h(tag, { "data-align": ALIGN.exec(open.attrGet("style") ?? "")?.[1] }, children));
      } else {
        push(h(tag, null, children));
      }
    } else if (token.type === "fence" || token.type === "code_block") {
      const language = token.info?.trim().split(/\s+/)[0] ?? "";
      const code = html`<${Code} source=${token.content.replace(/\n$/, "")} language=${language} />`;
      push(language.toLowerCase() === "mermaid" ? html`<${Diagram} source=${token.content} fallback=${code} />` : code);
    } else if (token.type === "hr") {
      push(html`<hr />`);
    } else {
      push(token.content);
    }
  }
  return stack[0].children;
}

// One parse per text: a timeline refresh that brings the same message reuses the nodes.
// `words`, when given, turns a text run into nodes (task chips); keep it the same function.
export function Markdown({ text, files, words }) {
  const granted = files?.filter((file) => file.image).map((file) => file.hash).join() ?? "";
  return useMemo(() => {
    const env = {};
    const tokens = parser.parse(text, env);
    // markdown-it keeps a link's address on the opening token only.
    for (const token of tokens) {
      let href = null;
      for (const child of token.children ?? []) {
        if (child.type === "link_open") href = child.attrGet("href") ?? "";
        else if (child.type === "link_close") child.meta = href;
      }
    }
    return html`<div class="markdown">${blocks(tokens, files, words)}</div>`;
  }, [text, granted, words]);
}
