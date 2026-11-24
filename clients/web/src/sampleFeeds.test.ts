import { test } from "node:test";
import assert from "node:assert/strict";
import { sampleEvents, sampleJobs } from "./sampleFeeds.ts";

test("sample jobs are the five from the mockup", () =>
  assert.deepEqual(sampleJobs.jobs().map((j) => j.name), ["Order recon", "Instrument sync", "Daily stats", "Reported staleness", "Spool prune"]));
test("runs come newest first", () => {
  const runs = sampleJobs.runs("Order recon");
  assert.ok(runs.length > 1);
  for (let i = 1; i < runs.length; i++) assert.ok(runs[i - 1].at > runs[i].at);
  assert.deepEqual(sampleJobs.runs("nope"), []);
});
test("events filter by stream", () =>
  assert.ok(sampleEvents.events("published", "").length > 0 && sampleEvents.events("published", "").every((e) => e.stream === "published")));
test("events filter by key, case-insensitively", () => {
  assert.ok(sampleEvents.events("bus", "ORD-8F21").length > 0);
  assert.ok(sampleEvents.events("bus", "ord-8f21").every((e) => [e.key, e.topic, e.summary].join(" ").toLowerCase().includes("ord-8f21")));
});
test("sources say they are samples", () => assert.ok(sampleJobs.sample && sampleEvents.sample));
