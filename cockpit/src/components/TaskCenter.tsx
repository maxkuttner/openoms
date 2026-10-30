import { useState } from "react";
import { Badge, Button, Code, Drawer, Group, Loader, Progress, Stack, Text, UnstyledButton } from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "../api/client";
import {
  JOB_KIND_LABEL,
  JOB_STATUS_COLOR,
  formatDuration,
  summarize,
  type JobDetail,
  type JobRecord,
} from "../api/jobs";
import { palette } from "../palette";

function JobLog({ id }: { id: string }) {
  const { data, isLoading } = useQuery<JobDetail>({
    queryKey: ["/admin/jobs", id],
    queryFn: () => api.get<JobDetail>(`/admin/jobs/${id}`),
    refetchInterval: (q) => (q.state.data && ["queued", "running"].includes(q.state.data.job.status) ? 2000 : false),
  });
  if (isLoading) return <Loader size="xs" />;
  if (!data?.log.length) return <Text fz="xs" c="dimmed">No log lines.</Text>;
  return (
    <Code block fz={11} style={{ maxHeight: 220, overflow: "auto" }}>
      {data.log.map((l) => `${new Date(l.at).toLocaleTimeString()} ${l.level.toUpperCase().padEnd(5)} ${l.message}`).join("\n")}
    </Code>
  );
}

function JobRow({ job }: { job: JobRecord }) {
  const [open, setOpen] = useState(false);
  const queryClient = useQueryClient();
  const total = job.progress_total ?? 0;
  const pct = total > 0 ? Math.min(100, ((job.progress_done ?? 0) / total) * 100) : 0;
  // A universe load is safe to repeat (it upserts); offer it where one did not finish.
  const canRunAgain =
    job.kind === "universe_load" && job.subject && (job.status === "failed" || job.status === "interrupted");

  async function runAgain() {
    try {
      await api.post(`/admin/broker-connections/${job.subject}/universe/load`, {});
      queryClient.invalidateQueries({ queryKey: ["/admin/jobs"] });
    } catch (err) {
      notifications.show({ color: "red", message: err instanceof ApiError ? err.message : String(err) });
    }
  }

  return (
    <Stack gap={4} py={8} style={{ borderBottom: `1px solid ${palette.border}` }}>
      <UnstyledButton onClick={() => setOpen((o) => !o)}>
        <Group justify="space-between" wrap="nowrap">
          <Stack gap={0}>
            <Text fz="sm" fw={600}>
              {JOB_KIND_LABEL[job.kind] ?? job.kind}
              {job.subject ? ` · ${job.subject}` : ""}
            </Text>
            <Text fz="xs" c="dimmed">
              {new Date(job.created_at).toLocaleString()} · {job.started_by}
              {job.started_at ? ` · ${formatDuration(job)}` : ""}
            </Text>
          </Stack>
          <Badge color={JOB_STATUS_COLOR[job.status]} variant="light">
            {job.status}
          </Badge>
        </Group>
      </UnstyledButton>
      {job.status === "running" && total > 0 && (
        <Group gap="xs" wrap="nowrap">
          <Progress value={pct} size="sm" style={{ flex: 1 }} />
          <Text fz="xs" c="dimmed">
            {(job.progress_done ?? 0).toLocaleString()} / {total.toLocaleString()}
          </Text>
        </Group>
      )}
      {summarize(job) && (
        <Text fz="xs" c={job.status === "failed" ? "red" : "dimmed"}>
          {summarize(job)}
        </Text>
      )}
      {canRunAgain && (
        <Group>
          <Button size="compact-xs" variant="light" onClick={runAgain}>
            Run again
          </Button>
        </Group>
      )}
      {open && <JobLog id={job.id} />}
    </Stack>
  );
}

/** Recent background jobs, newest first: status, progress, outcome, and each job's log. */
export function TaskCenter({ opened, onClose, jobs }: { opened: boolean; onClose: () => void; jobs: JobRecord[] }) {
  return (
    <Drawer opened={opened} onClose={onClose} position="right" size="md" title="Jobs">
      {jobs.length === 0 ? (
        <Text c="dimmed" fz="sm">
          No background jobs yet.
        </Text>
      ) : (
        jobs.map((j) => <JobRow key={j.id} job={j} />)
      )}
    </Drawer>
  );
}
