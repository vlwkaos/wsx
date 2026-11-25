// One button system (docs/web-themes.md "Buttons"): a button is drawn 36 px high (md) or 28 px
// (.button-sm, an action in a row or a card) with one radius per size, inside a 44 px hit box.
// The For you rows (docs/web-client-tasks.md "Project screen") follow the components board: the
// text sets the row's height, the last action of every row ends on one edge, the refusing action
// has no fill. Real tasks of the fake agent: NEEDS waits for an answer, a ready task waits to be
// landed, and the lead's `wsx task land` of a ready task asks the owner's approval.
import { test, expect, until } from "../fixtures.mjs";

const SIZES = { md: { height: 36, radius: 10 }, sm: { height: 28, radius: 8 } };
const HIT = 44;

/** Every shown button's box, drawn height and corner, by size. */
const buttons = (page) => page.evaluate(() => [...document.querySelectorAll(".button, .icon-button")]
  .filter((element) => element.getClientRects().length && getComputedStyle(element).visibility !== "hidden")
  .map((element) => {
    const style = getComputedStyle(element);
    const box = element.getBoundingClientRect();
    const drawn = box.height - parseFloat(style.borderTopWidth) - parseFloat(style.borderBottomWidth);
    return {
      name: element.getAttribute("aria-label") || element.textContent.trim() || element.className,
      icon: element.classList.contains("icon-button"),
      size: element.classList.contains("button-sm") ? "sm" : "md",
      box: Math.round(box.height), drawn: Math.round(drawn),
      // ^ The drawn corner: the horizontal radius of a word button, the radius less the side border of an icon button.
      radius: Math.round(parseFloat(style.borderTopLeftRadius) - (element.classList.contains("icon-button") ? parseFloat(style.borderLeftWidth) : 0)),
      fill: style.backgroundColor,
      // ^ The fill covers the drawn shape only: the background shorthand would reset the clip to the hit box.
      clip: style.backgroundClip,
    };
  }));

/** The open For you list's rows: the row, its text, its title's wrap and its actions' right edges. */
const rows = (page) => page.locator(".reservoir .need").evaluateAll((items) => items.map((item) => {
  const link = item.querySelector(".row-link");
  const main = link.querySelector(".row-main");
  const actions = [...link.querySelectorAll(".need-action")];
  const style = getComputedStyle(link);
  return {
    label: item.querySelector(".row-label")?.textContent,
    height: link.getBoundingClientRect().height,
    text: main.getBoundingClientRect().height + parseFloat(style.paddingTop) + parseFloat(style.paddingBottom),
    titleWraps: getComputedStyle(item.querySelector(".row-label")).whiteSpace !== "nowrap",
    right: actions.length ? Math.round(actions.at(-1).getBoundingClientRect().right) : null,
    actions: actions.map((action) => ({ word: action.title, text: action.textContent.trim(), fill: getComputedStyle(action).backgroundColor })),
  };
}));

test("buttons have two drawn sizes in one hit box, and the For you rows line up", async ({ wsx, leadChat: { page } }, testInfo) => {
  test.setTimeout(180_000);
  const state = (id) => wsx.json("task", "show", String(id)).state;
  for (const prompt of ["NEEDS Bash(curl:*) WORK m.txt", "WORK a.txt", "WORK b.txt"]) wsx.json("task", "new", prompt);
  for (const [id, want] of [[1, "waiting"], [2, "ready"], [3, "ready"]]) await until(() => state(id) === want, `#${id} ${want}`, 60);
  // ^ The lead lands #3: a land needs the owner's approval (land.auto is off in the fixture).
  wsx.json("chat", "say", "RUN wsx task land 3 --json");

  // ^ The lead (the main chat, no goal) reads "demo lead" in its top row and on Home, as its replies do.
  const tag = (selector) => page.locator(selector).evaluate((node) => `${node.textContent} ${getComputedStyle(node, "::after").content.replaceAll('"', "")}`);
  expect(await tag(".bar-label")).toBe("demo lead");
  await page.locator("button.pill[data-kind='you']").click();
  await expect(page.locator(".reservoir .need")).toHaveCount(4, { timeout: 60_000 });
  await expect(page.locator(".reservoir .need-action")).toHaveCount(5);
  await page.screenshot({ path: testInfo.outputPath("for-you.png") });

  const shown = await buttons(page);
  expect(shown.length).toBeGreaterThan(4);
  for (const button of shown) {
    const want = SIZES[button.size];
    expect.soft(button.box, `${button.name}: hit box`).toBe(HIT);
    expect.soft(button.drawn, `${button.name}: drawn height`).toBe(want.height);
    expect.soft(button.radius, `${button.name}: corner`).toBe(want.radius);
    expect.soft(button.clip, `${button.name}: fill`).toBe("padding-box");
  }

  const listed = await rows(page);
  // ^ Answer for #1, Land for #2 and #3, Deny and Approve for the land of #3 the lead asked for.
  const words = listed.flatMap((row) => row.actions.map((action) => action.word));
  expect(words.sort()).toEqual(["Answer", "Approve", "Deny", "Land", "Land"]);
  // ^ The actions are icons beside the text on every width: the word is the title only.
  for (const row of listed) {
    expect.soft(row.titleWraps, `${row.label}: the title is one line`).toBe(false);
    expect.soft(Math.abs(row.height - row.text), `${row.label}: the text sets the height`).toBeLessThanOrEqual(1);
    for (const action of row.actions) expect.soft(action.text, `${action.word} shows its icon only`).toBe("");
  }
  const edges = new Set(listed.map((row) => row.right));
  expect(edges.size, `the last actions end on ${[...edges]}`).toBe(1);
  const deny = listed.flatMap((row) => row.actions).find((action) => action.word === "Deny");
  expect(deny.fill).toBe("rgba(0, 0, 0, 0)");
  const accepting = listed.flatMap((row) => row.actions).filter((action) => action.word !== "Deny");
  for (const action of accepting) expect.soft(action.fill, `${action.word} keeps the raised fill`).not.toBe("rgba(0, 0, 0, 0)");
});
