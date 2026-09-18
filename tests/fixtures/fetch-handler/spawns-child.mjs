// The serve signal must not reach a child. This entry is itself a handler, and the
// child it spawns is the same file — so if the signal propagated, the child would
// bind a second port and report a second `Listening on` line.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

if (!process.env.FIXTURE_IS_CHILD) {
  const child = spawnSync(process.execPath, [fileURLToPath(import.meta.url)], {
    env: { ...process.env, FIXTURE_IS_CHILD: "1" },
    encoding: "utf8",
  });
  process.stdout.write(`child-stdout:${child.stdout.trim()}\n`);
  process.stdout.write(`child-stderr:${child.stderr.trim()}\n`);
} else {
  process.stdout.write("child ran to completion\n");
}

export default {
  fetch() {
    return new Response("parent handler");
  },
};
