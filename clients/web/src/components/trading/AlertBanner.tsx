import { useState } from "react";
import { useQueries } from "@tanstack/react-query";
import { CalendarClock } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import type { Me } from "../../access";
import { api } from "../../api/client";
import { useAckAlert, useAlerts } from "../../api/hooks";
import { useLive } from "../../api/liveUpdates";
import { bannerText, daysUntil, offerFromAlert, venueToday, visibleRollAlerts, type PositionLite } from "../../futures";
import type { PositionRow } from "../../positions";
import { RollDialog, type RollTarget } from "./RollDialog";

/** Roll-due alerts for held futures, above the trade screen. Dismiss acknowledges the alert; Roll opens the roll dialog. */
export function AlertBanner({ me }: { me: Me }) {
  const live = useLive();
  const alerts = useAlerts(live);
  const ack = useAckAlert();
  const [target, setTarget] = useState<RollTarget | null>(null);

  const rolls = (alerts.data ?? []).filter((a) => a.kind === "ROLL_DUE");
  const portfolioIds = [...new Set(rolls.map((a) => a.portfolio_id))];
  // Same query key as the Positions page, so the two share one fetch.
  const positionQueries = useQueries({
    queries: portfolioIds.map((id) => ({
      queryKey: ["/portfolios", id, "positions"],
      queryFn: () => api.get<PositionRow[]>(`/portfolios/${id}/positions`),
      refetchInterval: live ? (false as const) : 5000,
    })),
  });
  const held = new Map<string, PositionLite[] | null | undefined>();
  portfolioIds.forEach((id, i) => {
    const q = positionQueries[i];
    held.set(id, q.data ?? (q.isError ? null : undefined));
  });

  const visible = visibleRollAlerts(rolls, held);
  if (visible.length === 0) return <RollDialog target={target} onClose={() => setTarget(null)} />;
  const today = venueToday();
  const codeOf = (id: string) => me.portfolios.find((p) => p.portfolio_id === id)?.code ?? id;

  return (
    <>
      <div className="flex flex-col gap-2">
        {visible.map((a) => {
          const offer = offerFromAlert(a);
          const position = held.get(a.portfolio_id)?.find((p) => p.instrument_id === String(a.instrument_id));
          const netQty = position?.net_qty ?? (typeof a.payload.net_qty === "number" ? a.payload.net_qty : 0);
          const text = bannerText(
            {
              contract: offer?.nearSymbol ?? String(a.payload.contract ?? a.instrument_id),
              days: offer ? daysUntil(today, offer.rollDate) : 0,
              qty: netQty,
              portfolio: codeOf(a.portfolio_id),
            },
            netQty > 0,
          );
          return (
            <Alert key={a.id} className="flex items-center gap-2 border-warn/40">
              <CalendarClock className="size-4 shrink-0 text-warn" />
              <AlertDescription className="flex flex-1 flex-wrap items-center justify-between gap-2 text-foreground">
                <span>{text}</span>
                <span className="flex gap-2">
                  {offer && netQty !== 0 && (
                    <Button size="sm" onClick={() => setTarget({
                      portfolioId: a.portfolio_id,
                      portfolioCode: codeOf(a.portfolio_id),
                      netQty,
                      nearInstrumentId: offer.nearInstrumentId,
                      nearSymbol: offer.nearSymbol,
                      farInstrumentId: offer.farInstrumentId,
                      farSymbol: offer.farSymbol,
                    })}>Roll</Button>
                  )}
                  <Button size="sm" variant="ghost" disabled={ack.isPending} onClick={() => ack.mutate(a.id)}>Dismiss</Button>
                </span>
              </AlertDescription>
            </Alert>
          );
        })}
      </div>
      <RollDialog target={target} onClose={() => setTarget(null)} />
    </>
  );
}
