// Background jobs and broker connections' instrument universes (src/jobs/).
import { useQuery } from "@tanstack/react-query";
import { useCallback, useState } from "react";
import { api } from "./client";

export type JobStatus = "queued" | "running" | "succeeded" | "failed" | "interrupted";

export type JobRecord = {
  id: string;
  kind: string;
  subject: string | null;
  started_by: string;
  params: Record<string, unknown>;
  status: JobStatus;
  progress_done: number | null;
  progress_total: number | null;
  result: Record<string, unknown> | null;
  error: string | null;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
};

export type JobLogLine = { at: string; level: string; message: string };
export type JobDetail = { job: JobRecord; log: JobLogLine[] };

export type UniverseState = "not_loaded" | "loading" | "loaded" | "partial" | "failed";
export type UniverseStatus = {
  state: UniverseState;
  count: number;
  reason: string | null;
  last_job: JobRecord | null;
};

export const JOB_KIND_LABEL: Record<string, string> = {
  universe_load: "Instrument universe",
  enrichment: "FIGI enrichment",
  symbology_backfill: "Symbology backfill",
  expiry_sweep: "Expiry sweep",
};

export const JOB_STATUS_COLOR: Record<JobStatus, string> = {
  queued: "gray",
  running: "brand",
  succeeded: "green",
  failed: "red",
  interrupted: "amber",
};

export const isActive = (j: JobRecord) => j.status === "queued" || j.status === "running";

/** Ask the jobs indicator in the top bar to open the Task Center. */
export function openTaskCenter() {
  window.dispatchEvent(new Event("oms:open-task-center"));
}

const SEEN_KEY = "oms_jobs_seen_at";

function readSeen(): string {
  try {
    return localStorage.getItem(SEEN_KEY) ?? "";
  } catch {
    return "";
  }
}

/**
 * Recent jobs, polled every 2 s while any is active and every 15 s otherwise.
 * `failedSinceSeen` is true when a job failed after the Task Center was last
 * opened (remembered per browser).
 */
export function useJobs() {
  const { data } = useQuery<JobRecord[]>({
    queryKey: ["/admin/jobs"],
    queryFn: () => api.get<JobRecord[]>("/admin/jobs?limit=50"),
    refetchInterval: (q) => (q.state.data?.some(isActive) ? 2000 : 15000),
  });
  const jobs = data ?? [];
  const [seen, setSeen] = useState(readSeen);
  const markSeen = useCallback(() => {
    const now = new Date().toISOString();
    try {
      localStorage.setItem(SEEN_KEY, now);
    } catch {
      // Private mode or blocked storage: the marker simply resets next load.
    }
    setSeen(now);
  }, []);
  return {
    jobs,
    running: jobs.filter(isActive).length,
    failedSinceSeen: jobs.some((j) => j.status === "failed" && (j.finished_at ?? "") > seen),
    markSeen,
  };
}

export function formatDuration(job: JobRecord): string {
  if (!job.started_at) return "";
  const end = job.finished_at ? new Date(job.finished_at) : new Date();
  const s = Math.max(0, Math.round((end.getTime() - new Date(job.started_at).getTime()) / 1000));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
}

/** One line for a job's outcome, from its result or error. */
export function summarize(job: JobRecord): string {
  if (job.status === "failed") return job.error ?? "failed";
  if (job.status === "interrupted") return "interrupted by a restart";
  const r = job.result;
  if (!r) return "";
  if (job.kind === "universe_load" || job.kind === "enrichment") {
    const skipped = Number(r.skipped ?? 0);
    const reasons = Array.isArray(r.skipped_reasons) ? (r.skipped_reasons as string[]).join(", ") : "";
    return `${Number(r.mapped ?? 0).toLocaleString()} loaded` + (skipped ? `, ${skipped} skipped: ${reasons}` : "");
  }
  if (job.kind === "expiry_sweep") return `${r.expired ?? 0} expired, ${r.recomputed ?? 0} dated`;
  if (job.kind === "symbology_backfill") return `${r.stamped ?? 0} of ${r.scanned ?? 0} stamped`;
  return "";
}
