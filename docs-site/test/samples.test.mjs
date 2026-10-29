// The API reference's code samples are generated per operation from the spec
// (scripts/openapi-samples.mjs) as Python and Rust, against a local OMS with a
// bearer token — what a trader actually runs.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import { codeSamples, exampleFor } from "../scripts/openapi-samples.mjs";

const spec = JSON.parse(readFileSync(new URL("../../docs/openapi.json", import.meta.url), "utf8"));

test("submit gets cURL, Python and Rust samples, nothing else", () => {
  const samples = codeSamples(spec, "/orders/submit", "post", spec.paths["/orders/submit"].post);
  assert.deepEqual(samples.map((s) => s.lang), ["shell", "python", "rust"]);
  const [sh, py, rs] = samples.map((s) => s.source);
  assert.match(sh, /^curl --request POST/);
  assert.match(py, /requests\.post\(/);
  assert.match(rs, /reqwest::Client::new\(\)/);
  for (const src of [sh, py, rs]) {
    assert.ok(src.includes("http://localhost:3001/orders/submit"), "local URL");
    assert.ok(src.includes("Bearer <token>"), "bearer auth");
    assert.ok(src.includes('"time_in_force"'), "request body");
  }
});

test("path parameters get a concrete example value", () => {
  const op = spec.paths["/orders/{id}"].get;
  const [, py] = codeSamples(spec, "/orders/{id}", "get", op);
  assert.ok(py.source.includes("http://localhost:3001/orders/00000000-0000-0000-0000-000000000000"), py.source);
});

test("example values prefer the spec's own examples", () => {
  const body = exampleFor(spec, { $ref: "#/components/schemas/SubmitOrderRequest" });
  assert.equal(body.side, "buy");
  assert.equal(typeof body.order_id, "string");
});

test("an operation with no JSON response prints the status, not a parsed body", () => {
  // POST /orders/submit answers 204 No Content: response.json() would raise.
  const [, py, rs] = codeSamples(spec, "/orders/submit", "post", spec.paths["/orders/submit"].post).map((s) => s.source);
  assert.ok(!py.includes("response.json()"), py);
  assert.match(py, /print\(response\.status_code\)/);
  assert.ok(!rs.includes("json::<serde_json::Value>()"), rs);
  assert.match(rs, /\.status\(\)/);
});

test("an operation with a JSON response still parses it", () => {
  const [, py, rs] = codeSamples(spec, "/orders", "get", spec.paths["/orders"].get).map((s) => s.source);
  assert.ok(py.includes("print(response.json())"));
  assert.ok(rs.includes("json::<serde_json::Value>()"));
});
