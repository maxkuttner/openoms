import type { QueryClient } from "@tanstack/query-core";

// Refetching on live-update notices without cancelling anything.
//
// A plain invalidateQueries cancels a fetch already in flight. Notices can come
// every 250ms; an endpoint slower than that would be cancelled on every notice
// and its screen would never show new data. So an in-flight fetch is left to
// finish, and because it may have read the database before the change the
// notice is about, the query is refetched once more when it settles.
export function createRefresher(client: QueryClient) {
  const owed = new Set<string>();

  const stop = client.getQueryCache().subscribe((event) => {
    const query = event.query;
    if (event.type !== "updated" || !owed.has(query.queryHash) || query.state.fetchStatus !== "idle") return;
    owed.delete(query.queryHash);
    client.invalidateQueries({ queryKey: query.queryKey, exact: true }, { cancelRefetch: false });
  });

  const refresh = (stale: (queryKey: readonly unknown[]) => boolean) => {
    for (const query of client.getQueryCache().findAll({ predicate: (q) => stale(q.queryKey) })) {
      if (query.state.fetchStatus === "fetching") owed.add(query.queryHash);
    }
    client.invalidateQueries({ predicate: (q) => stale(q.queryKey) }, { cancelRefetch: false });
  };

  return { refresh, stop };
}
