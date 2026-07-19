import { useQuery } from "@tanstack/react-query";
import { Badge, Group, Loader, Paper, Stack, Text, Tooltip } from "@mantine/core";
import { api } from "../api/client";
import type { StreamHealth } from "../api/types";

const PATH = "/admin/stream-health";

const STATE_COLOR: Record<StreamHealth["state"], string> = {
  live: "green",
  connecting: "yellow",
  down: "red",
};

function ago(iso: string | null): string {
  if (!iso) return "never";
  const secs = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000));
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
  return `${Math.round(secs / 3600)}h ago`;
}

/**
 * Live WebSocket connection health, filtered to one stream kind. Polls every 5s;
 * state is process-local to the OMS (empty when no streams of that kind run).
 * `kind` = "execution" for broker order/fill streams, "feed" for market data.
 */
export function StreamHealthStrip({
  kind,
  title = "Live connections",
  emptyText = "No live streams in this process.",
}: {
  kind: StreamHealth["kind"];
  title?: string;
  emptyText?: string;
}) {
  const { data, isLoading, isError } = useQuery<StreamHealth[]>({
    queryKey: [PATH],
    queryFn: () => api.get<StreamHealth[]>(PATH),
    refetchInterval: 5000,
  });

  if (isLoading) return <Loader size="sm" />;
  if (isError) return null;
  const streams = (data ?? []).filter((s) => s.kind === kind);
  if (streams.length === 0) {
    return (
      <Text size="sm" c="dimmed">
        {emptyText}
      </Text>
    );
  }

  return (
    <Stack gap="xs">
      <Text size="sm" fw={600}>
        {title}
      </Text>
      <Group gap="sm">
        {streams.map((s) => (
          <Tooltip
            key={`${s.broker_code}/${s.environment}`}
            label={
              s.state === "down"
                ? `down${s.last_error ? `: ${s.last_error}` : ""} · last event ${ago(s.last_event_at)}`
                : `connected ${ago(s.connected_since)} · last event ${ago(s.last_event_at)}`
            }
          >
            <Paper withBorder p="xs" radius="md">
              <Group gap={8} wrap="nowrap">
                <Badge color={STATE_COLOR[s.state]} variant="filled" size="sm">
                  {s.state}
                </Badge>
                <Text size="sm" fw={500}>
                  {s.broker_code}
                </Text>
                <Text size="sm" c="dimmed">
                  {s.environment}
                </Text>
                <Text size="xs" c="dimmed">
                  {ago(s.last_event_at)}
                </Text>
              </Group>
            </Paper>
          </Tooltip>
        ))}
      </Group>
    </Stack>
  );
}
