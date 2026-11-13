import { Alert, Anchor, Stack, Text } from "@mantine/core";
import { Link } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import { checklistSteps, checklistVisible, type SetupStatus } from "../checklist";

export function useSetupStatus() {
  return useQuery<SetupStatus>({
    queryKey: ["/admin/setup-status"],
    queryFn: () => api.get<SetupStatus>("/admin/setup-status"),
    refetchInterval: 15000,
  });
}

/** First-run checklist; renders nothing once every step is done. */
export function SetupChecklist() {
  const { data } = useSetupStatus();
  if (!data || !checklistVisible(data)) return null;
  return (
    <Alert color="gray" title="Getting started">
      <Stack gap={4}>
        {checklistSteps(data).map((s) => (
          <Text key={s.key} size="sm" c={s.done ? "dimmed" : undefined} td={s.done ? "line-through" : undefined}>
            {s.done ? "✓ " : "○ "}
            <Anchor component={Link} to={s.to} inherit>{s.label}</Anchor>
          </Text>
        ))}
      </Stack>
    </Alert>
  );
}
