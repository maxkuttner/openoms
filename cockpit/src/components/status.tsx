import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

export type Tone = "buy" | "sell" | "warn" | "info" | "muted";

const TONE: Record<Tone, string> = {
  buy: "bg-buy/15 text-buy",
  sell: "bg-sell/15 text-sell",
  warn: "bg-warn/15 text-warn",
  info: "bg-accent text-foreground",
  muted: "bg-accent text-muted-foreground",
};

/** A small status pill. */
export function Pill({ tone = "muted", children, className }: { tone?: Tone; children: ReactNode; className?: string }) {
  return (
    <span className={cn("inline-flex items-center rounded px-1.5 py-px text-[11px] font-medium whitespace-nowrap", TONE[tone], className)}>
      {children}
    </span>
  );
}

/// One order-status vocabulary and colouring, wherever an order is shown.
export const ORDER_STATUS_TONE: Record<string, Tone> = {
  filled: "buy",
  partially_filled: "buy",
  routed: "info",
  submitted: "muted",
  canceled: "warn",
  rejected: "sell",
  expired: "warn",
};

export function OrderStatus({ status }: { status: string }) {
  return <Pill tone={ORDER_STATUS_TONE[status] ?? "muted"}>{status}</Pill>;
}

export function SideBadge({ side }: { side: string }) {
  return <Pill tone={side === "buy" ? "buy" : "sell"}>{side}</Pill>;
}
