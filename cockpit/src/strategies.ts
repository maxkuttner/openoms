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

// Whether a strategy trades the venue directly (the OMS follows its account).
export function isColocated(row: { mode?: string }): boolean {
  return row.mode === "COLOCATED";
}

// A co-located deployment as the admin API returns it (the fields shown).
export type Deployment = {
  id: string;
  status: string;
  epoch: number;
  policy_revision: number;
  acked_policy_revision: number | null;
  kill_epoch: number | null;
  kill_ack_stage: string | null;
  recovery_ready: boolean | null;
  recovery_missing: string | null;
  inventory_shortfall: string | null;
  created_at: string;
};

export type Health = { label: string; color: "green" | "orange" | "red" | "gray"; detail?: string };

// The deployment the runtime currently serves: the newest active one.
export function activeDeployment(rows: Deployment[] | undefined): Deployment | undefined {
  return (rows ?? [])
    .filter((d) => d.status === "ACTIVE")
    .sort((a, b) => b.created_at.localeCompare(a.created_at))[0];
}

const KILL_STAGES: Record<string, string> = {
  RECEIVED: "kill received",
  ADMISSION_STOPPED: "orders stopped",
  CANCELS_SENT: "cancels sent",
  CONFIRMED_CLEAR: "killed, nothing resting",
};

// What the runtime last reported, worst first. Empty fields mean it has not
// reported yet (never connected since registration).
export function deploymentHealth(d: Deployment): Health[] {
  const out: Health[] = [];
  if (d.inventory_shortfall) {
    out.push({ label: "balance short", color: "red", detail: d.inventory_shortfall });
  }
  if (d.recovery_ready === false) {
    out.push({ label: "recovery incomplete", color: "red", detail: d.recovery_missing ?? undefined });
  }
  if (d.kill_epoch !== null) {
    const stage = d.kill_ack_stage ? KILL_STAGES[d.kill_ack_stage] ?? d.kill_ack_stage : "kill not acknowledged";
    out.push({ label: stage, color: d.kill_ack_stage === "CONFIRMED_CLEAR" ? "orange" : "red" });
  }
  if (d.acked_policy_revision === null) {
    out.push({ label: "policy not acknowledged", color: "gray" });
  } else if (d.acked_policy_revision < d.policy_revision) {
    out.push({ label: `policy ${d.acked_policy_revision} of ${d.policy_revision}`, color: "orange" });
  }
  if (out.length === 0 && d.recovery_ready) out.push({ label: "healthy", color: "green" });
  if (d.recovery_ready === null) out.push({ label: "runtime never reported", color: "gray" });
  return out;
}
