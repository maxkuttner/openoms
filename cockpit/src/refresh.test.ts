// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { QueryClient, QueryObserver } from "@tanstack/query-core";
import { createRefresher } from "./trade/api/refresh.ts";

const settle = () => new Promise((r) => setTimeout(r, 10));

// Notices can arrive faster than a slow endpoint answers. Cancelling the
// in-flight fetch on each one would mean the screen never gets data at all;
// dropping them would miss a change the in-flight fetch read too early for.
test("notices during a fetch neither cancel it nor get lost", async () => {
  const client = new QueryClient();
  const answers: Array<(v: string) => void> = [];
  let calls = 0;
  const observer = new QueryObserver(client, {
    queryKey: ["/orders"],
    queryFn: () =>
      new Promise<string>((resolve) => {
        calls++;
        answers.push(resolve);
      }),
  });
  const unsubscribe = observer.subscribe(() => {});
  const refresher = createRefresher(client);
  await settle();
  answers[0]("initial");
  await settle();

  // a refetch is in flight on a query that already shows data — the case a
  // plain invalidate would cancel
  refresher.refresh((key) => key[0] === "/orders");
  await settle();
  assert.equal(calls, 2);
  refresher.refresh((key) => key[0] === "/orders");
  refresher.refresh((key) => key[0] === "/orders");
  await settle();
  assert.equal(calls, 2, "the in-flight fetch is kept, not restarted");

  answers[1]("second");
  await settle();
  assert.equal(calls, 3, "one trailing fetch for the notices that arrived meanwhile");

  answers[2]("third");
  await settle();
  assert.equal(calls, 3);
  assert.equal(observer.getCurrentResult().data, "third");

  refresher.stop();
  unsubscribe();
  client.clear();
});

test("a notice for an idle query refetches it once", async () => {
  const client = new QueryClient();
  let calls = 0;
  const observer = new QueryObserver(client, {
    queryKey: ["/marks", ["1"]],
    queryFn: async () => ++calls,
  });
  const unsubscribe = observer.subscribe(() => {});
  const refresher = createRefresher(client);
  await settle();

  refresher.refresh((key) => key[0] === "/marks");
  await settle();
  assert.equal(calls, 2);

  refresher.stop();
  unsubscribe();
  client.clear();
});
