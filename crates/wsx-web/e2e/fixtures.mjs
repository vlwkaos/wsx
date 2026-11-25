// The browser tests' apparatus (README.md): per worker one isolated wsx (its own HOME, state,
// wsxd, `wsx web serve` and the fake Claude, scripts/fake-claude-stream.py), as
// crates/wsx/tests/common/mod.rs builds it for the Rust tests; per test a fresh browser context
// paired with that site. Nothing here mocks wsx: the page talks to the real server.
import { test as base, expect } from "@playwright/test";
import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";

const ROOT = path.resolve(import.meta.dirname, "../../..");
const BIN = path.join(ROOT, "target/debug");

const freePort = () => new Promise((done, failed) => {
  const server = net.createServer().once("error", failed);
  server.listen(0, "127.0.0.1", () => {
    const { port } = server.address();
    server.close(() => done(port));
  });
});

async function until(test, label, seconds = 30) {
  const deadline = Date.now() + seconds * 1000;
  let last;
  while (Date.now() < deadline) {
    try {
      last = await test();
      if (last) return last;
    } catch (error) {
      last = error;
    }
    await new Promise((done) => setTimeout(done, 100));
  }
  throw new Error(`${label}: timeout (last: ${last instanceof Error ? last.message : JSON.stringify(last)})`);
}

/** Stop `child` and wait for its exit: SIGTERM, then SIGKILL after 5 s. */
function ended(child) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise((done) => {
    const kill = setTimeout(() => child.kill("SIGKILL"), 5000);
    child.once("exit", () => { clearTimeout(kill); done(); });
    child.kill("SIGTERM");
  });
}

let made = 0;

/** One isolated wsx with a project `demo` whose lead runs on the fake agent, served on loopback.
 *  A failed setup undoes what it made before it throws. */
async function makeWsx() {
  for (const binary of ["wsx", "wsxd"]) {
    if (!fs.existsSync(path.join(BIN, binary))) throw new Error(`missing ${binary}: cargo build -p wsx -p wsx-daemon first`);
  }
  // ^ Short and under target/: the daemon socket must stay below macOS SUN_LEN.
  const root = path.join(ROOT, "target", `e2e-${process.pid}-${made++}`);
  fs.rmSync(root, { recursive: true, force: true });
  const undo = [() => fs.rmSync(root, { recursive: true, force: true })];
  const stop = async () => {
    for (const step of undo.reverse()) {
      try { await step(); } catch { /* ^ the next step still runs */ }
    }
  };
  try {
    return await setUp(root, undo, stop);
  } catch (error) {
    await stop();
    throw error;
  }
}

async function setUp(root, undo, stop) {
  // ^ `wsx project add` takes a folder inside the home folder; its name is the project's.
  const project = path.join(root, "home/demo");
  const config = path.join(root, "home/Library/Application Support/wsx");
  for (const dir of [config, project, path.join(root, "bin"), path.join(root, "t")]) fs.mkdirSync(dir, { recursive: true });
  // ^ The daemon socket's folder must be the owner's alone.
  fs.chmodSync(root, 0o700);
  fs.writeFileSync(path.join(config, "config-v2.toml"), "resume_agents_on_restore = false\n");
  fs.writeFileSync(path.join(project, "wsx.config.yml"),
    "tasks:\n  minFreeGb: 0\n  land: { auto: false, approve: true }\n  agent:\n    model: fake-model\n  chats: { model: fake-lead }\n");
  const fake = path.join(root, "bin/claude");
  fs.writeFileSync(fake, `#!/bin/sh\nexec python3 ${JSON.stringify(path.join(ROOT, "scripts/fake-claude-stream.py"))} "$@"\n`, { mode: 0o755 });

  const env = Object.fromEntries(Object.entries(process.env).filter(([name]) => !name.startsWith("WSX_")));
  Object.assign(env, {
    HOME: path.join(root, "home"),
    XDG_STATE_HOME: path.join(root, "state"),
    XDG_CONFIG_HOME: path.join(root, "config"),
    XDG_CACHE_HOME: path.join(root, "cache"),
    TMPDIR: path.join(root, "t"),
    PATH: `${path.join(root, "bin")}:${process.env.PATH}`,
    SHELL: "/bin/sh",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@example.com",
    GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@example.com",
    WSX_DAEMON_BIN: path.join(BIN, "wsxd"),
    WSX_SOCKET: path.join(root, "d.sock"),
    // ^ wsxd ends with this worker, also when the worker is killed.
    WSX_EXIT_WITH_PID: String(process.pid),
  });
  // ^ Bounded: a hung wsx call fails the test instead of holding the worker.
  const run = (program, args, input) => execFileSync(program, args, { cwd: project, env, input, encoding: "utf8", stdio: ["pipe", "pipe", "pipe"], timeout: 30_000 });
  const cli = (...args) => run(path.join(BIN, "wsx"), args);
  const json = (...args) => JSON.parse(cli(...args, "--json"));
  run("git", ["init", "-q", "--template=", "-b", "main"]);
  run("git", ["add", "."]);
  run("git", ["commit", "-q", "-m", "start"]);
  // ^ `wsx web serve` connects to wsxd and does not start it.
  const daemonLog = fs.openSync(path.join(root, "wsxd.log"), "a");
  undo.push(() => fs.closeSync(daemonLog));
  const daemon = spawn(path.join(BIN, "wsxd"), [], { cwd: project, env, stdio: ["ignore", daemonLog, daemonLog] });
  undo.push(() => ended(daemon));
  // ^ Its sessions' programs (agent hosts) end with it.
  undo.push(() => cli("daemon", "stop", "--force"));
  await until(() => fs.existsSync(env.WSX_SOCKET), "wsxd listens");
  // ^ Registers it and tells wsxd: the page lists wsxd's projects, which learns them only from a client.
  cli("project", "add", project);
  // ^ A repo has no chat of its own: the project's first chat is its main chat (docs/tasks.md "Chats").
  cli("chat", "new", "demo");

  const site = path.join(root, "site.toml");
  const log = fs.openSync(path.join(root, "web.log"), "a");
  undo.push(() => fs.closeSync(log));
  let origin;
  // ^ A free port can be taken by another worker before the server binds it: a server that
  // exits at its start tries again on another.
  for (let attempt = 1; ; attempt += 1) {
    const port = await freePort();
    origin = `http://127.0.0.1:${port}`;
    // ^ docs/web-client.md "Site file": owner-only, mode 600.
    fs.writeFileSync(site, [
      'id = "e2e"', 'name = "wsx e2e"', `port = ${port}`, `public_origin = "${origin}"`, 'panels = ["chat", "terminal"]', 'theme = "default"',
      // ^ Every project and a terminal: the site manages projects (docs/web-client.md "Projects").
      "[access]", 'projects = "all"', 'capabilities = ["chat_send", "media_upload", "task_view", "task_act", "terminal_view", "terminal_input"]', "",
    ].join("\n"), { mode: 0o600 });
    const server = spawn(path.join(BIN, "wsx"), ["web", "serve", "--site", site], { cwd: project, env, stdio: ["ignore", log, log] });
    undo.push(() => ended(server));
    const up = await until(async () => server.exitCode !== null ? "exited" : (await fetch(`${origin}/manifest.webmanifest`)).ok && "up", "the web server starts");
    if (up === "up") break;
    if (attempt === 3) throw new Error(`the web server exited at its start three times; see ${path.join(root, "web.log")}`);
  }

  return {
    root, home: path.join(root, "home"), project, origin, cli, json, run,
    /** A one-time code that pairs one browser with the site. */
    pairCode: () => cli("web", "pair", "--site", site).split("\n")[0].trim().split(" ").at(-1),
    /** The owner says `text` to the lead; resolves with the lead's reply entry. */
    async say(text) {
      const { id } = json("chat", "say", text);
      return until(() => json("chat", "read").entries.find((entry) => entry.kind === "reply" && entry.to?.includes(id)), `the lead's reply to ${text}`);
    },
    stop,
  };
}

export const test = base.extend({
  // ^ One per test: each test starts from an empty chat, also its retry (about 1 s to set up).
  wsx: async ({}, use) => {
    const wsx = await makeWsx();
    try {
      await use(wsx);
    } finally {
      await wsx.stop();
    }
  },

  /** A page paired with the test's site, at demo's lead chat. */
  leadChat: async ({ page, wsx }, use) => {
    await page.goto(`${wsx.origin}/`);
    await page.locator("#pair-code").fill(wsx.pairCode());
    await page.locator("#pair-code").press("Enter");
    await expect(page.locator(".app")).toBeVisible();
    // ^ GET api/tasks: one overview per granted project (docs/web-client-tasks.md); demo is the only one.
    const overviews = await page.evaluate(() => fetch("api/tasks").then((r) => r.json()));
    if (!Array.isArray(overviews) || overviews.length !== 1 || !overviews[0].main_chat) {
      throw new Error(`api/tasks gave ${JSON.stringify(overviews).slice(0, 400)}`);
    }
    await page.evaluate((hash) => { location.hash = hash; }, `#/c/${overviews[0].main_chat}`);
    await expect(page.locator(".log")).toBeVisible();
    await use({ page, log: page.locator(".log") });
  },
});

export { expect, until };
