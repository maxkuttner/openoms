import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { NumberInput, Select } from "../form";
import { api } from "../../api/client";
import { useFutureRoots, useSubmitRoll } from "../../api/hooks";
import { clampQty, defaultQty, defaultSpreadPrice, rollLegs, spreadTickOf } from "../../futures";
import type { MarkRow } from "./types";

export interface RollTarget {
  portfolioId: string;
  portfolioCode: string;
  /** Futures root (ES): its spread tick prices the roll. */
  root: string;
  /** Signed position in the near contract. */
  netQty: number;
  nearInstrumentId: number;
  nearSymbol: string;
  farInstrumentId: number;
  farSymbol: string;
}

const TYPES = [{ value: "limit", label: "Limit" }, { value: "market", label: "Market" }];

/** Roll a futures position: one spread order (sell near/buy far for a long) the OMS books as two leg orders. */
export function RollDialog({ target, onClose }: { target: RollTarget | null; onClose: () => void }) {
  return (
    <Dialog open={target !== null} onOpenChange={(o) => { if (!o) onClose(); }}>
      {target && <RollForm key={`${target.portfolioId}:${target.nearInstrumentId}`} target={target} onClose={onClose} />}
    </Dialog>
  );
}

function RollForm({ target, onClose }: { target: RollTarget; onClose: () => void }) {
  const { netQty, nearInstrumentId, farInstrumentId } = target;
  const [qty, setQty] = useState<number | string>(defaultQty(netQty));
  const [type, setType] = useState<string>("limit");
  const [price, setPrice] = useState<number | string>("");
  const [edited, setEdited] = useState(false);

  const ids = [nearInstrumentId, farInstrumentId];
  const marks = useQuery<MarkRow[]>({
    queryKey: ["/marks", ids.map(String)],
    queryFn: () => api.get<MarkRow[]>(`/marks?instrument_ids=${ids.join(",")}`),
  });
  const mid = (id: number) => marks.data?.find((m) => m.instrument_id === id)?.mid ?? null;
  const tick = spreadTickOf(target.root, useFutureRoots().data ?? []);
  const suggested = defaultSpreadPrice(netQty, mid(nearInstrumentId), mid(farInstrumentId), tick);
  // Follow the marks until the trader types their own price.
  useEffect(() => {
    if (!edited && suggested !== null) setPrice(suggested);
  }, [suggested, edited]);

  const roll = useSubmitRoll(onClose);
  const qtyNum = clampQty(typeof qty === "number" ? qty : Number.NaN, netQty);
  const priceNum = typeof price === "number" ? price : null;
  const limit = type === "limit";
  const qtyError =
    qty === "" || typeof qty !== "number" ? undefined
    : !Number.isInteger(qty) ? "Whole contracts only"
    : qty > Math.abs(netQty) ? `At most ${Math.abs(netQty)}` : undefined;
  const ready = qtyNum !== null && qtyError === undefined && (!limit || priceNum !== null) && !roll.isPending;

  function submit() {
    if (qtyNum === null) return;
    roll.mutate({
      portfolio_id: target.portfolioId,
      from_instrument_id: nearInstrumentId,
      to_instrument_id: farInstrumentId,
      qty: qtyNum,
      order_type: limit ? "limit" : "market",
      ...(limit && priceNum !== null ? { limit_price: priceNum } : {}),
      time_in_force: "day",
    });
  }

  return (
    <DialogContent className="sm:max-w-md">
      <DialogHeader>
        <DialogTitle>Roll {target.nearSymbol} → {target.farSymbol}</DialogTitle>
        <DialogDescription>{target.portfolioCode}</DialogDescription>
      </DialogHeader>
      <p className="num font-medium" data-testid="roll-legs">{rollLegs(netQty, target.nearSymbol, target.farSymbol, qtyNum ?? defaultQty(netQty))}</p>
      <div className="grid grid-cols-2 gap-3">
        <NumberInput label="Quantity" value={qty} onChange={setQty} min={1} max={Math.abs(netQty)} step={1} error={qtyError} />
        <Select label="Type" data={TYPES} value={type} onChange={(v) => v && setType(v)} />
        {limit && (
          <NumberInput className="col-span-2" label={`Spread price (net debit per spread${tick ? `, tick ${tick}` : ""})`} value={price} step={tick ?? "any"}
            onChange={(v) => { setEdited(true); setPrice(v); }}
            placeholder={marks.isLoading ? "Loading marks…" : "No mid available — enter a price"} />
        )}
      </div>
      <DialogFooter>
        <Button variant="ghost" onClick={onClose}>Cancel</Button>
        <Button disabled={!ready} onClick={submit}>Roll</Button>
      </DialogFooter>
    </DialogContent>
  );
}
