// Jobs and Events behind small interfaces. There is no job scheduler, message-bus
// listing or Kafka publishing yet, so the only implementations are fixed samples;
// screens fed from a source with `sample: true` show a "Sample data" badge.

export type JobStatus = "ok" | "running" | "failed" | "paused";
export type ScheduledJob = { name: string; description: string; cron: string; lastRun: string | null; nextRun: string; status: JobStatus };
export type JobRun = { at: string; status: JobStatus; durationMs: number | null; note: string };
export interface JobsSource { sample: boolean; jobs(): ScheduledJob[]; runs(name: string): JobRun[] }

export type BusEvent = { at: string; topic: string; key: string; summary: string; stream: "bus" | "published" };
export interface EventsSource { sample: boolean; events(stream: BusEvent["stream"], q: string): BusEvent[] }

const JOBS: ScheduledJob[] = [
  { name: "Order recon", description: "Snapshot venue orders and match", cron: "*/10 * * * *", lastRun: "2026-11-23T10:40:00Z", nextRun: "2026-11-23T10:50:00Z", status: "running" },
  { name: "Instrument sync", description: "Refresh instruments from venues", cron: "0 2 * * *", lastRun: "2026-11-23T02:00:04Z", nextRun: "2026-11-24T02:00:00Z", status: "ok" },
  { name: "Daily stats", description: "Close prices and day P&L", cron: "*/5 * * * *", lastRun: "2026-11-23T10:40:00Z", nextRun: "2026-11-23T10:45:00Z", status: "ok" },
  { name: "Reported staleness", description: "Flag silent reported strategies", cron: "* * * * *", lastRun: "2026-11-23T10:42:00Z", nextRun: "2026-11-23T10:43:00Z", status: "failed" },
  { name: "Spool prune", description: "Drop acknowledged batches older than 7 days", cron: "0 3 * * *", lastRun: "2026-11-23T03:00:01Z", nextRun: "2026-11-24T03:00:00Z", status: "paused" },
];

const RUNS: Record<string, JobRun[]> = {
  "Order recon": [
    { at: "2026-11-23T10:40:00Z", status: "running", durationMs: null, note: "Binance, IBKR" },
    { at: "2026-11-23T10:30:00Z", status: "ok", durationMs: 4200, note: "0 mismatches" },
    { at: "2026-11-23T10:20:00Z", status: "ok", durationMs: 3900, note: "0 mismatches" },
    { at: "2026-11-23T10:10:00Z", status: "failed", durationMs: 30000, note: "IBKR timeout" },
    { at: "2026-11-23T10:00:00Z", status: "ok", durationMs: 4100, note: "1 mismatch fixed" },
  ],
  "Reported staleness": [
    { at: "2026-11-23T10:42:00Z", status: "failed", durationMs: 120, note: "mm7: no report for 3 h" },
    { at: "2026-11-23T10:41:00Z", status: "ok", durationMs: 95, note: "all fresh" },
  ],
};

export const sampleJobs: JobsSource = {
  sample: true,
  jobs: () => JOBS,
  runs: (name) => RUNS[name] ?? [],
};

const EVENTS: BusEvent[] = [
  { at: "2026-11-23T10:42:11.204Z", topic: "order.filled", key: "ord-8f21", summary: "BTCUSDT BUY 0.01 @ 61,004.5", stream: "bus" },
  { at: "2026-11-23T10:42:11.198Z", topic: "fill.booked", key: "ord-8f21", summary: "Booked into MAIN", stream: "bus" },
  { at: "2026-11-23T10:42:10.951Z", topic: "order.acked", key: "ord-8f21", summary: "Venue accepted", stream: "bus" },
  { at: "2026-11-23T10:42:10.902Z", topic: "order.new", key: "ord-8f21", summary: "Limit BUY 0.01 BTCUSDT", stream: "bus" },
  { at: "2026-11-23T10:41:03.440Z", topic: "order.partial", key: "ord-7c09", summary: "ETHUSDT SELL 0.10 of 0.25", stream: "bus" },
  { at: "2026-11-23T10:40:00.000Z", topic: "job.run", key: "recon-orders", summary: "Order recon started", stream: "bus" },
  { at: "2026-11-23T10:39:12.333Z", topic: "reported.batch", key: "abc-1a2b3c4d5e6f", summary: "5 fills, 5 breaks", stream: "bus" },
  { at: "2026-11-23T10:42:11.210Z", topic: "oms.fills", key: "ord-8f21", summary: "Published to Kafka", stream: "published" },
  { at: "2026-11-23T10:42:10.960Z", topic: "oms.orders", key: "ord-8f21", summary: "Published to Kafka", stream: "published" },
  { at: "2026-11-23T10:41:03.450Z", topic: "oms.fills", key: "ord-7c09", summary: "Published to Kafka", stream: "published" },
];

export const sampleEvents: EventsSource = {
  sample: true,
  events(stream, q) {
    const needle = q.trim().toLowerCase();
    return EVENTS.filter((e) => e.stream === stream && (!needle || [e.key, e.topic, e.summary].join(" ").toLowerCase().includes(needle)));
  },
};
