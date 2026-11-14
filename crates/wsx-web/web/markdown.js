// Markdown in agent messages (docs/web-client-chat.md "Markdown").
// ^ markdown-it only parses: this file builds the page's nodes from its token list.
// No HTML string from a message is ever given to the DOM, raw HTML in a message stays
// text, and a message cannot load a remote resource.
import "./markdown-it.js";
import hljs from "./highlight.js";
import { resolve } from "./api.js";
import { h } from "./preact.js";
import { html, useMemo, useState } from "./lib.js";
import { Diagram } from "./diagram.js";

// ^ A table whose header sits right under a list item's line, not indented to the item
// (docs/design/list-tables.md): markdown-it's table rule needs the delimiter row at the item's
// indent, so those lines stayed text of the item. This rule ends the item's paragraph at such a
// header, by asking the table rule itself with the indent of the page; the table then follows
// the list, as GitHub shows it with a blank line between.
const afterItem = (table) => (state, line, end, silent) => {
  if (!silent || state.sCount[line] >= state.blkIndent) return false;
  const indent = state.blkIndent;
  state.blkIndent = 0;
  try {
    return table(state, line, end, true);
  } finally {
    state.blkIndent = indent;
  }
};

// ^ GFM turns a line without `|` right under a table into a row; a manager's closing sentence
// then became a one-cell row that widened the first column. A table ends at such a line, as the
// TUI chat reads it: the rows markdown-it took for it are given back to the text after it.
const endingAtText = (table) => (state, line, end, silent) => {
  const found = table(state, line, end, silent);
  if (!found || silent) return found;
  const { tokens } = state;
  const hasPipe = (at) => state.src.slice(state.bMarks[at] + state.tShift[at], state.eMarks[at]).includes("|");
  while (state.line > line + 2 && !hasPipe(state.line - 1)) {
    const row = tokens.findLastIndex((token) => token.type === "tr_open");
    tokens.splice(row, tokens.findLastIndex((token) => token.type === "tbody_close") - row);
    state.line -= 1;
  }
  tokens.findLast((token) => token.type === "table_open").map[1] = state.line;
  const body = tokens.findLast((token) => token.type === "tbody_open");
  if (body) body.map[1] = state.line;
  return true;
};

// ^ CommonMark lets a `*` next to punctuation open and close emphasis, so a glob an agent wrote
// without backticks (`mmproj-*.gguf and mtp-*.gguf`) turned its text between the stars to italics.
// A `*` or `_` run opens only before a letter, a digit or a mark that wraps words (`, [, (, a quote,
// another `*` or `_`) and closes only after one of those or a sentence mark (`**Note:**`). The
// delimiters markdown-it's emphasis rule pushed are cleared here, so the pairing rule that follows
// pairs the rest as before.
const OPENS = /[\p{L}\p{N}`\[("'“‘*_~]/u;
const CLOSES = /[\p{L}\p{N}`\])"'”’*_~.,;:!?]/u;
const wordEdges = (emphasis) => (state, silent) => {
  const start = state.pos;
  const first = state.delimiters.length;
  const found = emphasis(state, silent);
  if (!found || silent) return found;
  const before = state.src[start - 1] ?? "";
  for (const delim of state.delimiters.slice(first)) {
    delim.open &&= OPENS.test(state.src[start + delim.length] ?? "");
    delim.close &&= CLOSES.test(before);
  }
  return found;
};

const makeParser = () => {
  const made = globalThis.markdownit({ html: false, linkify: true });
  const emphasis = made.inline.ruler.__rules__.find((rule) => rule.name === "emphasis").fn;
  made.inline.ruler.at("emphasis", wordEdges(emphasis));
  const table = endingAtText(made.block.ruler.__rules__.find((rule) => rule.name === "table").fn);
  made.block.ruler.at("table", table, { alt: ["paragraph", "reference"] });
  made.block.ruler.before("table", "table_after_item", afterItem(table), { alt: ["paragraph"] });
  // Only text with a scheme becomes a link: `main.rs` and `notes.io` are file names here.
  made.linkify.set({ fuzzyLink: false, fuzzyEmail: false });
  return made;
};
const parser = makeParser();
// ^ Task text (reports, briefs, the manager chat, the project overview) is prose a model wrote:
// a paragraph indented by four spaces stays a paragraph. Only a fence (```) is code there.
const proseParser = makeParser().disable("code");

const LINK = /^(https?:|mailto:)/i;

// The file the server granted for a `files/<path>` address (docs/web-client.md "Media"): it
// names the path it was granted for, so a remote address or another folder's file of the same
// name is never it.
const grantedFile = (files, href) => {
  if (!href?.startsWith("files/")) return undefined;
  let path;
  try {
    path = decodeURIComponent(href.slice(6).split(/[?#]/)[0]);
  } catch {
    return undefined;
  }
  return files?.find((file) => file.src === path);
};
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

function inline(tokens, files, words, relative) {
  const stack = [[]];
  const push = (node) => stack.at(-1).push(node);
  let linked = 0;
  for (const token of tokens ?? []) {
    if (token.nesting === 1) {
      if (token.type === "link_open") linked += 1;
      stack.push([]);
    } else if (token.nesting === -1) {
      const children = stack.pop();
      if (token.type === "link_close") {
        linked -= 1;
        const href = token.meta;
        // A relative address opens the page the view's `relative` gives it (`files/<path>`,
        // `session/<id>`); any other scheme, and another relative address, stays text.
        const file = grantedFile(files, href);
        const page = !file && relative && !href.includes("..") ? relative(href) : null;
        // ^ A link to a file the server granted keeps its words: the file is an attachment of the
        // message (chat.js `Attachments`).
        push(file ? children
          : page ? html`<a href=${page}>${children}</a>`
          : LINK.test(href) ? html`<a href=${href} target="_blank" rel="noopener noreferrer">${children}</a>` : children);
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
      // ^ A file the server granted by its path is an attachment of the message, shown under it
      // (chat.js `Attachments`), so the syntax shows nothing here. Any other image is its alt
      // text, except a picture an agent produced (it has no path): the file of that name.
      const src = token.attrGet("src") ?? "";
      if (grantedFile(files, src)) continue;
      const name = src.split(/[?#]/)[0].split("/").at(-1);
      const file = files?.find((candidate) => !candidate.src && candidate.image && candidate.name === name);
      const picture = file && html`<img src=${resolve(file.url)} alt=${token.content} loading="lazy" />`;
      push(file ? (linked ? picture : html`<a href=${resolve(file.url)} data-view>${picture}</a>`)
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

// The words of rendered inline nodes, for a header named in another cell.
const plain = (nodes) => [nodes].flat(Infinity).map((node) => typeof node === "string" ? node : plain(node?.props?.children ?? "")).join("");

function blocks(tokens, files, words, relative) {
  const stack = [{ children: [] }];
  const push = (...nodes) => stack.at(-1).children.push(...nodes);
  for (const token of tokens) {
    if (token.type === "inline") {
      push(...inline(token.children, files, words, relative));
    } else if (token.nesting === 1) {
      // The closing token carries no attributes: keep the opening one.
      stack.push({ open: token, children: [] });
    } else if (token.nesting === -1) {
      const frame = stack.pop();
      const { open, children } = frame;
      const tag = open.tag;
      if (tag === "p") {
        // A tight list hides its paragraphs.
        if (open.hidden) push(children);
        else if (!children.every((node) => typeof node === "string" && !node.trim())) push(html`<p>${children}</p>`);
      } else if (tag === "li") {
        const done = task(children);
        push(done === null ? html`<li>${children}</li>`
          : html`<li class="task"><input type="checkbox" checked=${done} disabled aria-label=${done ? "Done" : "Not done"} />${children}</li>`);
      } else if (tag === "ol") {
        push(h("ol", { start: open.attrGet("start") ?? undefined }, children));
      } else if (tag === "table") {
        // ^ On a phone a table of more than three columns keeps two; the others are lines of the
        // second cell, each named by its header (app.css, docs/web-client-chat.md "Markdown").
        const fold = (frame.heads?.length ?? 0) > 3 ? "" : null;
        push(html`<div class="table-wrap" tabindex="0" data-fold=${fold}><table>${children}</table></div>`);
      } else if (tag === "th" || tag === "td") {
        const row = stack.at(-1);
        const table = stack.findLast((other) => other.open?.tag === "table");
        const column = row.cells = (row.cells ?? 0) + 1;
        if (tag === "th") (table.heads ??= []).push(plain(children));
        push(h(tag, { "data-align": ALIGN.exec(open.attrGet("style") ?? "")?.[1], "data-head": tag === "td" && column > 2 ? table.heads?.[column - 1] : undefined }, children));
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
// `relative(href)`, when given, is the address of a relative link or none; keep it the same function.
// `prose` reads indented lines as text, not code (docs/web-client-chat.md "Markdown").
export function Markdown({ text, files, words, relative, prose = false }) {
  const granted = files?.map((file) => file.hash + (file.src ?? "")).join() ?? "";
  return useMemo(() => {
    const env = {};
    const tokens = (prose ? proseParser : parser).parse(text, env);
    // markdown-it keeps a link's address on the opening token only.
    for (const token of tokens) {
      let href = null;
      for (const child of token.children ?? []) {
        if (child.type === "link_open") href = child.attrGet("href") ?? "";
        else if (child.type === "link_close") child.meta = href;
      }
    }
    return html`<div class="markdown" data-gallery>${blocks(tokens, files, words, relative)}</div>`;
  }, [text, granted, words, relative, prose]);
}
