import assert from "node:assert/strict";
import { once } from "node:events";
import express from "express";
const app = express();
app.use(express.json());
app.post("/double/:n", (req, res) => res.json({ value: Number(req.params.n) * req.body.factor }));
const server = app.listen(0, "127.0.0.1");
try {
  await once(server, "listening");
  const response = await fetch(`http://127.0.0.1:${server.address().port}/double/21`, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ factor: 2 }),
  });
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { value: 42 });
  console.log("ok:http-42");
} finally {
  await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
}
