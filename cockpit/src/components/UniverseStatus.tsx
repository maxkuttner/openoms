import { Anchor, Button, Group, Progress, Stack, Text, Tooltip } from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "../api/client";
import { openTaskCenter, type UniverseStatus as Status } from "../api/jobs";

const SYMBOL: Record<Status["state"], { sym: string; color: string; label: string }> = {
  not_loaded: { sym: "○", color: "dimmed", label: "Not loaded" },
  loading: { sym: "◐", color: "yellow", label: "Loading" },
  loaded: { sym: "●", color: "green", label: "Loaded" },
  partial: { sym: "◑", color: "orange", label: "Partial" },
  failed: { sym: "✕", color: "red", label: "Failed" },
};

const ACTION: Partial<Record<Status["state"], string>> = {
  not_loaded: "Load",
  loaded: "Reload",
  partial: "Load again",
  failed: "Retry",
};

/** Start (or restart) a connection's universe load. A 409 means one is already running. */
export async function loadUniverse(code: string): Promise<boolean> {
  try {
    await api.post(`/admin/broker-connections/${code}/universe/load`, {});
    notifications.show({
      color: "teal",
      message: (
        <>
          Loading the instrument universe for {code} in the background.{" "}
          <Anchor component="button" fz="sm" onClick={openTaskCenter}>
            Open Jobs
          </Anchor>
        </>
      ),
    });
    return true;
  } catch (err) {
    if (err instanceof ApiError && err.status === 409) {
      notifications.show({ color: "yellow", message: "Already loading." });
      openTaskCenter();
    } else {
      notifications.show({ color: "red", message: err instanceof ApiError ? err.message : String(err) });
    }
    return false;
  }
}

/** The "Instrument universe" cell for one broker connection. */
export function UniverseStatus({ code }: { code: string }) {
  const qc = useQueryClient();
  const { data } = useQuery<Status>({
    queryKey: ["/admin/broker-connections", code, "universe"],
    queryFn: () => api.get<Status>(`/admin/broker-connections/${code}/universe`),
    refetchInterval: (q) => (q.state.data?.state === "loading" ? 2000 : 15000),
  });
  // Load needs usable credentials; the server refuses without them (422), the
  // button says why before anyone clicks.
  const { data: creds } = useQuery<{ state: string }>({
    queryKey: [`/admin/broker-connections/${code}/credentials`],
    queryFn: () => api.get<{ state: string }>(`/admin/broker-connections/${code}/credentials`),
  });
  if (!data) return <Text fz="sm" c="dimmed">…</Text>;

  const s = SYMBOL[data.state];
  const job = data.last_job;
  const total = job?.progress_total ?? 0;
  const done = job?.progress_done ?? 0;
  const action = ACTION[data.state];
  const configured = creds?.state === "configured";

  async function run() {
    if (await loadUniverse(code)) {
      qc.invalidateQueries({ queryKey: ["/admin/broker-connections", code, "universe"] });
      qc.invalidateQueries({ queryKey: ["/admin/jobs"] });
    }
  }

  return (
    <Group justify="space-between" wrap="nowrap" gap="sm">
      <Stack gap={2}>
        <Text fz="sm">
          <Text span c={s.color}>{s.sym} {s.label}</Text>
          {data.state === "loading" && total > 0 && ` — ${done.toLocaleString()} of ${total.toLocaleString()}`}
          {data.count > 0 && data.state !== "loading" && ` — ${data.count.toLocaleString()} instruments`}
        </Text>
        {data.state === "loading" && total > 0 && <Progress value={(done / total) * 100} size="xs" w={180} />}
        {data.state === "loaded" && job?.finished_at && (
          <Text fz="xs" c="dimmed">loaded {new Date(job.finished_at).toLocaleString()}</Text>
        )}
        {data.reason && (
          <Text fz="xs" c={data.state === "failed" ? "red" : "dimmed"} lineClamp={2} maw={360}>
            {data.reason}
          </Text>
        )}
      </Stack>
      {action && (
        <Tooltip label="Add credentials first" disabled={configured}>
          <Button size="compact-xs" variant="light" onClick={run} disabled={!configured}>
            {action}
          </Button>
        </Tooltip>
      )}
    </Group>
  );
}
