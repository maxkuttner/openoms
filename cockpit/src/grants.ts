// Trading grants: what a portfolio is allowed to use. The server stores the
// full active set per portfolio; the dialog edits it as one list.
export type Grant = { kind: "venue"; params: { connection: string; account_ref: string | null } };

export type GrantConnection = { code: string; broker_code: string; environment: string };

// IBKR connections reach several accounts, so the grant names which one.
export function needsRef(brokerCode: string): boolean {
  return brokerCode === "IBKR" || brokerCode === "IBKR_TWS";
}

export function venueGrants(picked: Set<string>, refs: Record<string, string>, brokers: Record<string, string>): Grant[] {
  return [...picked].sort().map((connection) => ({
    kind: "venue" as const,
    params: {
      connection,
      account_ref: needsRef(brokers[connection] ?? "") ? (refs[connection] ?? "").trim() : null,
    },
  }));
}

// Picked connections that need an account but have none typed yet.
export function missingRefs(connections: GrantConnection[], picked: Set<string>, refs: Record<string, string>): string[] {
  return connections
    .filter((c) => picked.has(c.code) && needsRef(c.broker_code) && !(refs[c.code] ?? "").trim())
    .map((c) => c.code);
}
