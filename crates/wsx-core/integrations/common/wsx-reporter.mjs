// Shared reporter boundary. See docs/agent-reporting.md.
import { execFile } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

function executable(candidate, uid) {
  const entry = fs.lstatSync(candidate);
  const resolved = fs.realpathSync(candidate);
  const target = fs.statSync(resolved);
  const parent = fs.statSync(path.dirname(resolved));
  if (![0, uid].includes(entry.uid) || !target.isFile() ||
      ![0, uid].includes(target.uid) || (target.mode & 0o022) !== 0 ||
      (target.mode & 0o111) === 0 || !parent.isDirectory() ||
      ![0, uid].includes(parent.uid) || (parent.mode & 0o022) !== 0) return undefined;
  return resolved;
}

function recovery(binary, env) {
  const root = env.XDG_STATE_HOME ||
    (env.HOME ? path.join(env.HOME, ".local/state") : undefined);
  const socket = env.WSX_SOCKET || (root ? path.join(root, "wsx/wsx.sock") : undefined);
  if (!socket || !path.isAbsolute(socket)) return undefined;
  const parsed = path.parse(socket);
  const stable = path.join(parsed.dir, `${parsed.name}.reporter`);
  const uid = process.getuid();
  const directory = fs.lstatSync(parsed.dir);
  if (!directory.isDirectory() || directory.uid !== uid || (directory.mode & 0o077) !== 0) return undefined;
  try {
    const entry = fs.lstatSync(stable);
    // An invalid entry is a refusal, not permission to bypass it through PATH.
    if (!entry.isSymbolicLink() || entry.uid !== uid) return undefined;
    const resolved = executable(stable, uid);
    if (!resolved || binary === resolved) return undefined;
    return resolved;
  } catch (error) {
    if (error.code !== "ENOENT") return undefined;
  }
  // An older, handoff-blocked daemon may not have published the stable entry,
  // or its previous keg may have been removed. Resolve the installed CLI once.
  for (const directory of (env.PATH || "").split(path.delimiter).slice(0, 32)) {
    if (!path.isAbsolute(directory)) continue;
    const candidate = path.join(directory, "wsx");
    try {
      const resolved = executable(candidate, uid);
      if (resolved && resolved !== binary) return resolved;
    } catch {
      // An absent or inaccessible PATH entry is not executable authority.
    }
  }
  return undefined;
}

export function execReporter(binary, args, options, callback) {
  execFile(binary, args, options, (error, stdout, stderr) => {
    // Retry only spawn ENOENT, never a report rejection or uncertain delivery.
    if (error?.code !== "ENOENT" || !path.isAbsolute(binary)) {
      callback(error, stdout, stderr);
      return;
    }
    let fallback;
    try {
      fallback = recovery(binary, options.env || process.env);
    } catch {
      // Filesystem failure leaves the original error intact.
    }
    if (!fallback) {
      callback(error, stdout, stderr);
      return;
    }
    execFile(fallback, args, options, callback);
  });
}
