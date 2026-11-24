import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { Section } from "../components/common";
import { SampleBadge } from "../components/market/SampleBadge";
import { Pill } from "../components/status";
import { fmtLocal } from "../tradeTimes";
import { sampleJobs, type JobStatus, type JobsSource } from "../sampleFeeds";

const TONE: Record<JobStatus, "buy" | "warn" | "sell" | "muted"> = { ok: "buy", running: "warn", failed: "sell", paused: "muted" };

function NotWired({ children }: { children: React.ReactNode }) {
  return (
    <Tooltip>
      <TooltipTrigger asChild><span tabIndex={0}>{children}</span></TooltipTrigger>
      <TooltipContent>Not wired yet</TooltipContent>
    </Tooltip>
  );
}

/** Scheduled jobs and their runs (sample until a scheduler exists). */
export function JobsPage({ source = sampleJobs }: { source?: JobsSource }) {
  const jobs = source.jobs();
  const [selected, setSelected] = useState(jobs[0]?.name ?? "");
  const runs = source.runs(selected);
  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-2">
        {source.sample && <SampleBadge />}
        <div className="ml-auto"><NotWired><Button size="sm" disabled>New job</Button></NotWired></div>
      </div>
      <Section className="p-0">
        <Table>
          <TableHeader>
            <TableRow><TableHead>Job</TableHead><TableHead>Schedule</TableHead><TableHead>Last run</TableHead><TableHead>Next run</TableHead><TableHead>Status</TableHead><TableHead /></TableRow>
          </TableHeader>
          <TableBody>
            {jobs.map((j) => (
              <TableRow key={j.name} className={cn("cursor-pointer", j.name === selected && "bg-accent")} onClick={() => setSelected(j.name)}>
                <TableCell>
                  <button type="button" className="font-medium" onClick={() => setSelected(j.name)}>{j.name}</button>
                  <div className="text-xs text-muted-foreground">{j.description}</div>
                </TableCell>
                <TableCell className="num text-muted-foreground">{j.cron}</TableCell>
                <TableCell className="num">{j.lastRun ? fmtLocal(j.lastRun) : "—"}</TableCell>
                <TableCell className="num">{fmtLocal(j.nextRun)}</TableCell>
                <TableCell><Pill tone={TONE[j.status]}>{j.status}</Pill></TableCell>
                <TableCell className="text-right"><NotWired><Button size="xs" variant="secondary" disabled>Run now</Button></NotWired></TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </Section>
      <Section title={`Runs — ${selected}`}>
        {runs.length === 0 ? <p className="text-sm text-muted-foreground">No runs recorded.</p> : (
          <Table>
            <TableBody>
              {runs.map((r) => (
                <TableRow key={r.at}>
                  <TableCell className="num text-muted-foreground">{fmtLocal(r.at)}</TableCell>
                  <TableCell><Pill tone={TONE[r.status]}>{r.status}</Pill></TableCell>
                  <TableCell className="num">{r.durationMs === null ? "—" : `${(r.durationMs / 1000).toFixed(1)} s`}</TableCell>
                  <TableCell className="text-muted-foreground">{r.note}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </Section>
    </div>
  );
}
