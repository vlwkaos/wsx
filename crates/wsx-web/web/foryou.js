// The For you page: what waits for the owner in every chat, grouped by chat (docs/web-client-tasks.md
// "Home"). The rows are the For you pill's (tasks.js `NeedRow`, `CheckingRow`) from `api/tasks/for-you`.
// An answer, a reply or the words of a task open the chat at the item: its For you list shows it marked.
import { html, Loading, plural } from "./lib.js";
import { Back } from "./ui.js";
import { chatHash, update, useStore } from "./store.js";
import { useForYou } from "./task-api.js";
import { CheckingRow, itemKey, NeedRow } from "./tasks.js";
import { needing } from "./task-words.js";
import { forYouWords } from "./trays.js";

const none = () => {};

export function ForYou() {
  const store = useStore();
  const chats = useForYou();
  const { total } = forYouWords(chats ?? []);
  // ^ The chat opens with its For you list at the item (`state.look`, read by the chat).
  const open = (entry, key) => {
    update({ look: { project: entry.project, room: entry.main ? null : entry.chat, key } });
    location.hash = chatHash(entry.chat);
  };
  let body;
  if (!chats) body = html`<${Loading} />`;
  else if (!total) body = html`<p class="empty">Nothing waits for you.</p>`;
  else {
    body = html`
      <div class="reservoir foryou">
        <ul class="reservoir-list" aria-label="For you">
          ${chats.map((entry) => {
            const project = store.projects.find((each) => each.id === entry.project);
            const items = needing(entry);
            const checks = entry.check_items ?? [];
            const to = (key) => () => open(entry, key);
            return html`
              <li class="reservoir-band" data-band="chat" key=${`band-${entry.project}-${entry.chat}`}>${project?.name} › ${entry.shown ?? entry.chat}</li>
              ${items.map((item, index) => html`<${NeedRow} key=${`${entry.project}-${itemKey(item)}`} project=${entry.project} item=${item} index=${index} onActing=${none} onFailed=${none}
                onMention=${to(itemKey(item))} onReply=${to(itemKey(item))} onShow=${to(itemKey(item))} />`)}
              ${checks.map((item, index) => html`<${CheckingRow} key=${`${entry.project}-${item.id}`} project=${entry.project} item=${item} index=${index} onActing=${none} onFailed=${none}
                onReply=${to(item.id)} />`)}`;
          })}
        </ul>
      </div>`;
  }
  return html`
    <div class="page manager">
      <header class="bar">
        <${Back} fallback="#/" />
        <div class="bar-title">
          <span class="bar-label">For you</span>
          ${total > 0 && html`<span class="bar-state">${plural(total, "item")}</span>`}
        </div>
      </header>
      <div class="scroll">${body}</div>
    </div>`;
}
