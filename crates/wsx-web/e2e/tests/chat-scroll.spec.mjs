// Following the end (docs/web-client-chat.md "Following the end"): useFollowEnd in
// crates/wsx-web/web/lib.js owns the log's scroll. Real replies from the fake lead (TABLE5 is tall,
// DIAGRAM grows after it shows, SLOW answers after about 3 s), a real finger on the phone and the
// wheel on the desktop.
import { test, expect } from "../fixtures.mjs";

const AT_END_PX = 80;

/** The log's numbers and the first row that starts in view, with its distance from the log's top. */
const view = (log) => log.evaluate((element) => {
  const rows = [...element.children].filter((child) => child.dataset.key);
  // ^ The row the log keeps in place (lib.js `mark`): the first that starts in view, else the last.
  const row = rows.find((child) => child.offsetTop >= element.scrollTop) ?? rows.at(-1);
  return {
    top: element.scrollTop,
    fromEnd: element.scrollHeight - element.scrollTop - element.clientHeight,
    key: row?.dataset.key ?? null,
    offset: row ? row.offsetTop - element.scrollTop : null,
  };
});

/** Scroll the log up by about `px` as the reader does: a finger drag down on a touch screen, else the wheel. */
async function readUp(page, log, px, hasTouch) {
  const box = await log.boundingBox();
  const x = box.x + box.width / 2;
  if (hasTouch) {
    const cdp = await page.context().newCDPSession(page);
    const from = box.y + box.height * 0.3;
    const point = (y) => [{ x, y, id: 1 }];
    await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: point(from) });
    // ^ Slow steps: no fling, so the log ends where the finger stops.
    for (let step = 1; step <= 20; step += 1) {
      await cdp.send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: point(from + (px * step) / 20) });
      await page.waitForTimeout(16);
    }
    await page.waitForTimeout(100);
    await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
    await cdp.detach();
  } else {
    await page.mouse.move(x, box.y + box.height / 2);
    await page.mouse.wheel(0, -px);
  }
}

/** The log is still: its height and scroll held for ten frames. */
const still = (log) => expect.poll(() => log.evaluate((element) => new Promise((done) => {
  const before = [element.scrollHeight, element.scrollTop];
  let frames = 10;
  const next = () => (--frames ? requestAnimationFrame(next) : done(element.scrollHeight === before[0] && element.scrollTop === before[1]));
  requestAnimationFrame(next);
}))).toBe(true);

/** The diagram of the last reply drew: its image loaded and the log is still. */
async function drawn(log) {
  // ^ Mermaid loads and draws in the page: under three workers it took over 10 s once.
  await expect.poll(() => log.locator("img[src^='blob:']").last()
    .evaluate((image) => image.complete && image.naturalHeight > 0).catch(() => false), { timeout: 30_000 }).toBe(true);
  await still(log);
}

/** Enough history to scroll: three tall replies. */
async function history(wsx) {
  for (let n = 0; n < 3; n += 1) await wsx.say("TABLE5");
}

test("a reader at the end follows a reply that grows after it shows", async ({ wsx, leadChat: { page, log } }) => {
  await history(wsx);
  await expect.poll(async () => (await view(log)).fromEnd).toBeLessThan(2);
  await wsx.say("DIAGRAM");
  // ^ The diagram draws after its bubble shows: the log grows twice.
  await drawn(log);
  expect((await view(log)).fromEnd).toBeLessThan(2);
  await expect(page.getByRole("button", { name: /Jump to latest/ })).toHaveCount(0);
});

// ^ The chat shows its last 60 rows (chat.js RECENT_ROWS): past that, each row that comes drops the
// oldest above the view, and the browser's CSS anchor moves the log up by it in the same frame.
test("a reader at the end of a long chat stays at the end as old rows leave the top", async ({ wsx, leadChat: { log } }) => {
  for (let n = 0; n < 31; n += 1) await wsx.say("NOOP");
  await still(log);
  expect((await view(log)).fromEnd).toBeLessThan(2);
  for (const words of ["TABLE5", "NOOP", "DIAGRAM"]) {
    await wsx.say(words);
    await still(log);
    expect((await view(log)).fromEnd, `after ${words}`).toBeLessThan(2);
  }
});

test("a reader who scrolled up keeps the row read while a reply arrives and grows", async ({ wsx, leadChat: { page, log }, hasTouch }) => {
  await history(wsx);
  await expect.poll(async () => (await view(log)).fromEnd).toBeLessThan(2);
  await readUp(page, log, 400, hasTouch);
  await expect.poll(async () => (await view(log)).fromEnd).toBeGreaterThan(AT_END_PX);
  const before = await view(log);
  await wsx.say("DIAGRAM");
  await drawn(log);
  // ^ The row read is where it was on the screen, also after the rows below changed.
  const offset = await log.evaluate((element, key) => {
    const row = [...element.children].find((child) => child.dataset.key === key);
    return row.offsetTop - element.scrollTop;
  }, before.key);
  expect(Math.abs(offset - before.offset)).toBeLessThanOrEqual(1);
  expect((await view(log)).fromEnd).toBeGreaterThan(AT_END_PX);
  await expect(page.getByRole("button", { name: /Jump to latest|new/ })).toBeVisible();
});

// ^ A reply that grows the log while the reader moves. It does not time the row change before the
// reader's scroll event (lib.js `readerMoved`): real replies cannot be held to one task.
test("a reader who scrolls up while a reply is coming is not pulled back to the end", async ({ wsx, leadChat: { page, log }, hasTouch }) => {
  await history(wsx);
  await expect.poll(async () => (await view(log)).fromEnd).toBeLessThan(2);
  const reply = wsx.say("SLOW TABLE5");
  await readUp(page, log, 400, hasTouch);
  // ^ The wheel scrolls over the next frames: the reader's place is where the scroll lands.
  await expect.poll(async () => (await view(log)).fromEnd).toBeGreaterThan(AT_END_PX);
  const left = await view(log);
  await reply;
  await expect(log.locator(".bubble").last()).toBeVisible();
  const after = await view(log);
  expect(after.fromEnd).toBeGreaterThan(AT_END_PX);
  expect(after.key).toBe(left.key);
});

test("opening a task's sheet from a chip keeps the chat where the reader left it", async ({ wsx, leadChat: { page, log } }) => {
  const { id } = wsx.json("task", "new", "NOOP");
  // ^ `#<n>` in any text is a chip that opens the task's sheet over the chat (docs/web-client-tasks.md).
  await wsx.say(`NOOP about #${id}`);
  await history(wsx);
  const chip = log.locator(".task-chip", { hasText: `#${id}` }).first();
  await chip.scrollIntoViewIfNeeded();
  await expect.poll(async () => (await view(log)).fromEnd).toBeGreaterThan(AT_END_PX);
  const top = (await view(log)).top;
  await chip.click();
  await expect(page.locator("dialog.task-sheet[open]")).toBeVisible();
  expect((await view(log)).top).toBe(top);
  await page.keyboard.press("Escape");
  await expect(page.locator("dialog.task-sheet[open]")).toHaveCount(0);
  expect((await view(log)).top).toBe(top);
});

/** A PNG of `width` x `height` drawn in the page, as a file the composer takes. */
const png = (page, name, width, height) => page.evaluate(([width, height]) => {
  const canvas = Object.assign(document.createElement("canvas"), { width, height });
  const context = canvas.getContext("2d");
  context.fillStyle = "#3a7"; context.fillRect(0, 0, width, height);
  return canvas.toDataURL("image/png").split(",")[1];
}, [width, height]).then((data) => ({ name, mimeType: "image/png", buffer: Buffer.from(data, "base64") }));

test("closing an image opened from the chat leaves the chat where it was", async ({ wsx, leadChat: { page, log }, hasTouch }) => {
  await history(wsx);
  // ^ The owner sends two images through the composer, as from a phone's photo picker.
  const files = [{ name: "notes.txt", mimeType: "text/plain", buffer: Buffer.from("notes for the lead") },
    await png(page, "one.png", 600, 900), await png(page, "two.png", 900, 600)];
  const [chooser] = await Promise.all([page.waitForEvent("filechooser"), page.getByRole("button", { name: "Attach files" }).click()]);
  await chooser.setFiles(files);
  await expect(page.locator(".composer .chips li")).toHaveCount(3);
  await expect(page.locator(".composer .chips [data-uploading]")).toHaveCount(0);
  await page.locator(".composer-field").fill("NOOP here are the pictures");
  await page.locator(".composer-send").click();
  const link = log.locator(".bubble[data-side=out] .attachments a[data-view]").first();
  await expect(link).toBeVisible();
  await expect(log.locator(".bubble").last()).toContainText(/./);
  // ^ The reader stayed at the end: the sent message and the reply that replaced its rows are followed.
  await expect.poll(async () => (await view(log)).fromEnd).toBeLessThan(2);
  // ^ The reply's rows still settle after it shows: the place is read once the log is still.
  await still(log);
  // ^ The image is brought into view and tapped at once, before the scroll's event comes: the
  // viewer opens over a log whose last move is not marked yet.
  const aim = await link.evaluate((element) => {
    element.scrollIntoView({ block: "nearest" });
    const box = element.getBoundingClientRect();
    return { x: box.left + box.width / 2, y: box.top + box.height / 2, top: element.closest(".log").scrollTop };
  });
  const top = aim.top;
  if (hasTouch) await page.touchscreen.tap(aim.x, aim.y);
  else await page.mouse.click(aim.x, aim.y);
  await expect(page.locator("dialog.viewer")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.locator("dialog.viewer")).toHaveCount(0);
  expect((await view(log)).top).toBe(top);
});
