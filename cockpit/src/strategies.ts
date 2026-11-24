// Pure helpers for the Strategies page, kept apart from React so they can be
// tested with node --test.

export type ControlReport = {
  cancelled: number;
  cancel_failures: string[];
  flatten_orders: number;
  skipped: string[];
};

export function killLabel(state: string): "Kill" | "Enable" {
  return state === "HALTED" ? "Enable" : "Kill";
}

export function reportSummary(r: ControlReport): string {
  const parts = [`cancelled ${r.cancelled}`];
  if (r.flatten_orders > 0) parts.push(`flatten orders ${r.flatten_orders}`);
  if (r.cancel_failures.length > 0) parts.push(`failed: ${r.cancel_failures.join("; ")}`);
  if (r.skipped.length > 0) parts.push(`skipped: ${r.skipped.join("; ")}`);
  return parts.join(", ");
}

type AccountLike = { id: string; broker_connection_code: string };
type ConnectionLike = { code: string; broker_code: string };

// The registry's connections for the picked accounts: each account with the
// broker its connection is on.
export function connectionEntries(
  accountIds: string[],
  accounts: AccountLike[],
  connections: ConnectionLike[],
): { broker_code: string; account_id: string }[] {
  return accountIds.flatMap((id) => {
    const account = accounts.find((a) => a.id === id);
    const connection = connections.find((c) => c.code === account?.broker_connection_code);
    return account && connection ? [{ broker_code: connection.broker_code, account_id: id }] : [];
  });
}

// The accounts a strategy's saved connections point at, to start the editor from.
export function savedAccounts(entries: { account_id: string }[] | undefined): string[] {
  return (entries ?? []).map((e) => e.account_id);
}
