// Add project (docs/web-client.md "Projects"): the field lists what the TUI's `p` lists, the git
// repositories under the host's home and, from / or ~, a path's folders; under the field it says
// what the typed folder is, and Add is on only for a repository that is not a project yet. From
// New chat the sheet goes back to New chat with the new project chosen. The fixture's home holds
// demo (a project), work/research-notes and research/alpha (repositories) and plain (a folder).
import fs from "node:fs";
import path from "node:path";
import { test, expect } from "../fixtures.mjs";

/** A git repository with one commit at `folder`. */
function repository(wsx, folder) {
  fs.mkdirSync(folder, { recursive: true });
  fs.writeFileSync(path.join(folder, "README.md"), "notes\n");
  for (const args of [["init", "-q", "-b", "main"], ["add", "."], ["commit", "-q", "-m", "start"]]) wsx.run("git", ["-C", folder, ...args]);
}

/** The listed rows: name, path and its word (git, folder, added). */
const listed = (page) => page.locator(".add-project .slash-option").evaluateAll((rows) => rows.map((row) => ({
  name: row.querySelector(".slash-name").textContent, path: row.querySelector(".slash-description").textContent,
  word: row.querySelector(".slash-source").textContent })));

test("Add project from New chat lists the repositories, says what a folder is and comes back with it chosen", async ({ wsx, leadChat: { page } }, testInfo) => {
  repository(wsx, path.join(wsx.home, "work/research-notes"));
  // ^ Its path starts with the query and its name does not: the name match comes first.
  repository(wsx, path.join(wsx.home, "research/alpha"));
  fs.mkdirSync(path.join(wsx.home, "plain/inside"), { recursive: true });

  await page.evaluate(() => { location.hash = "#/"; });
  await page.getByRole("button", { name: "New chat" }).first().click();
  await page.locator("dialog[open] select[name=project]").selectOption("add");
  const sheet = page.locator("dialog.add-project[open]");
  await expect(sheet).toBeVisible();
  const field = sheet.getByRole("combobox", { name: "Name or path" });
  const add = sheet.getByRole("button", { name: "Add", exact: true });

  // ^ Empty field: the scanned repositories; demo is already a project.
  await expect.poll(() => listed(page)).toEqual(expect.arrayContaining([
    { name: "research-notes", path: "~/work/research-notes/", word: "git" },
    { name: "demo", path: "~/demo/", word: "added" },
  ]));
  await expect(add).toBeDisabled();

  // ^ A path: its folders; a plain folder cannot be added, and says why.
  await field.fill("~/pl");
  await expect.poll(() => listed(page)).toEqual([{ name: "plain", path: "~/plain/", word: "folder" }]);
  await field.press("Tab");
  await expect(field).toHaveValue("~/plain/");
  await expect(sheet.locator(".folder-card")).toContainText("~/plain/ is not a git repository");
  await expect(add).toBeDisabled();

  // ^ The project already added says so.
  await field.fill("~/demo");
  await expect(sheet.locator(".folder-card")).toContainText("is already the project demo");
  await expect(add).toBeDisabled();

  // ^ A name matches the scan; Tab puts its path in; the line says what it is and where it shows.
  await field.fill("resea");
  await expect.poll(() => listed(page).then((rows) => rows.map((row) => row.name))).toEqual(["research-notes", "alpha"]);
  await page.screenshot({ path: testInfo.outputPath("add-project-list.png") });
  await field.press("Tab");
  await expect(field).toHaveValue("~/work/research-notes/");
  const card = sheet.locator(".folder-card");
  await expect(card).toContainText("research-notes");
  await expect(card).toContainText("git · main · 1 worktree");
  await expect(card).toContainText("Shows in Sessions with its worktrees, and on Home as its lead chat.");
  await page.screenshot({ path: testInfo.outputPath("add-project.png") });
  await expect(add).toBeEnabled();
  await add.click();

  // ^ Back in New chat, the new project chosen once the workspace lists it.
  const chat = page.locator("dialog[open]:has(#new-chat-title)");
  await expect(chat).toBeVisible();
  await expect(chat.locator("select[name=project] option:checked")).toHaveText("research-notes");
  await expect(chat).toContainText("Added just now");
  // ^ Saved in the owner's wsx config, as the TUI's `p` saves it.
  const config = fs.readFileSync(path.join(wsx.home, "Library/Application Support/wsx/config-v2.toml"), "utf8");
  expect([...config.matchAll(/^name = "([^"]+)"/gm)].map((match) => match[1]).sort()).toEqual(["demo", "research-notes"]);
});
