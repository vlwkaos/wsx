// Shared reporter boundary. See docs/agent-reporting.md.
import { execFile } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

export function execReporter(binary, args, options, callback) {
  execFile(binary, args, options, (error, stdout, stderr) => {
    // Retry only spawn ENOENT, never a report rejection or uncertain delivery.
    if (error?.code !== "ENOENT" || !path.isAbsolute(binary)) {
      callback(error, stdout, stderr);
      return;
    }
    const root = process.env.XDG_STATE_HOME ||
      (process.env.HOME ? path.join(process.env.HOME, ".local/state") : undefined);
    const socket = process.env.WSX_SOCKET || (root ? path.join(root, "wsx/wsx.sock") : undefined);
    if (!socket || !path.isAbsolute(socket)) {
      callback(error, stdout, stderr);
      return;
    }
    const parsed = path.parse(socket);
    const stable = path.join(parsed.dir, `${parsed.name}.reporter`);
    if (binary === stable) {
      callback(error, stdout, stderr);
      return;
    }
    try {
      const uid = process.getuid();
      const directory = fs.lstatSync(parsed.dir);
      const entry = fs.lstatSync(stable);
      const target = fs.statSync(stable);
      if (!directory.isDirectory() || directory.uid !== uid || (directory.mode & 0o077) !== 0 ||
          !entry.isSymbolicLink() || entry.uid !== uid || !target.isFile() ||
          ![0, uid].includes(target.uid) || (target.mode & 0o022) !== 0 || (target.mode & 0o111) === 0) {
        callback(error, stdout, stderr);
        return;
      }
    } catch {
      callback(error, stdout, stderr);
      return;
    }
    execFile(stable, args, options, callback);
  });
}
