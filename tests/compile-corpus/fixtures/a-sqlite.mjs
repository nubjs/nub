import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import Database from "better-sqlite3";
const root = mkdtempSync(join(tmpdir(), "compile-sqlite-"));
let db;
try {
  const file = join(root, "test.sqlite");
  db = new Database(file);
  db.pragma("journal_mode = WAL");
  db.exec("CREATE TABLE entries (amount INTEGER NOT NULL)");
  const insert = db.prepare("INSERT INTO entries VALUES (?)");
  db.transaction(() => { insert.run(10); insert.run(20); })();
  assert.throws(db.transaction(() => { insert.run(100); throw new Error("rollback"); }), /rollback/);
  db.close();
  db = new Database(file);
  assert.deepEqual(db.prepare("SELECT count(*) AS count, sum(amount) AS total FROM entries").get(), { count: 2, total: 30 });
  console.log("ok:sqlite-persistence-rollback");
} finally {
  db?.close();
  rmSync(root, { recursive: true, force: true });
}
