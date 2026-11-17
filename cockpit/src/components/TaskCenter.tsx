import { useState } from "react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { Spinner } from "./common";
import { notify } from "./notify";
import { Drawer } from "./overlay";
import { Pill } from "./status";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "../api/client";
import {
  JOB_KIND_LABEL,
  JOB_STATUS_TONE,
  formatDuration,
  summarize,
  type JobDetail,
  type JobRecord,
} from "../api/jobs";

function JobLog({ id }: { id: string }) {
  const { data, isLoading } = useQuery<JobDetail>({
    queryKey: ["/admin/jobs", id],
    queryFn: () => api.get<JobDetail>(`/admin/jobs/${id}`),
    refetchInterval: (q) => (q.state.data && ["queued", "running"].includes(q.state.data.job.status) ? 2000 : false),
  });
  if (isLoading) return <Spinner />;
  if (!data?.log.length) return <p className="text-xs text-muted-foreground">No log lines.</p>;
  return (
    <pre className="num max-h-56 overflow-auto rounded-md border bg-background p-2 text-[11px]">
      {data.log.map((l) => `${new Date(l.at).toLocaleTimeString()} ${l.level.toUpperCase().padEnd(5)} ${l.message}`).join("\n")}
    </pre>
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
      notify({ color: "red", message: err instanceof ApiError ? err.message : String(err) });
    }
  }

  return (
    <div className="flex flex-col gap-1 border-b py-2">
      <button type="button" onClick={() => setOpen((o) => !o)} className="flex items-center justify-between gap-2 text-left">
        <span className="flex flex-col">
          <span className="font-semibold">
            {JOB_KIND_LABEL[job.kind] ?? job.kind}
            {job.subject ? ` · ${job.subject}` : ""}
          </span>
          <span className="num text-xs text-muted-foreground">
            {new Date(job.created_at).toLocaleString()} · {job.started_by}
            {job.started_at ? ` · ${formatDuration(job)}` : ""}
          </span>
        </span>
        <Pill tone={JOB_STATUS_TONE[job.status]}>{job.status}</Pill>
      </button>
      {job.status === "running" && total > 0 && (
        <div className="flex items-center gap-2">
          <div className="h-1.5 flex-1 overflow-hidden rounded bg-accent">
            <div className="h-full bg-buy" style={{ width: `${pct}%` }} />
          </div>
          <span className="num text-xs text-muted-foreground">
            {(job.progress_done ?? 0).toLocaleString()} / {total.toLocaleString()}
          </span>
        </div>
      )}
      {summarize(job) && (
        <p className={cn("text-xs", job.status === "failed" ? "text-sell" : "text-muted-foreground")}>{summarize(job)}</p>
      )}
      {canRunAgain && (
        <Button size="xs" variant="secondary" className="self-start" onClick={runAgain}>Run again</Button>
      )}
      {open && <JobLog id={job.id} />}
    </div>
  );
}

/** Recent background jobs, newest first: status, progress, outcome, and each job's log. */
export function TaskCenter({ opened, onClose, jobs }: { opened: boolean; onClose: () => void; jobs: JobRecord[] }) {
  return (
    <Drawer open={opened} onClose={onClose} title="Jobs" className="sm:max-w-md">
      {jobs.length === 0 ? (
        <p className="text-muted-foreground">No background jobs yet.</p>
      ) : (
        jobs.map((j) => <JobRow key={j.id} job={j} />)
      )}
    </Drawer>
  );
}
