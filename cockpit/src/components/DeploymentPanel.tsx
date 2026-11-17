import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { FormRow } from "./common";
import { Modal } from "./overlay";
import { Pill, type Tone } from "./status";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import { notifyError, notifyOk } from "../api/hooks";
import { activeDeployment, deploymentHealth, type Deployment, type Health } from "../strategies";

const HEALTH_TONE: Record<Health["color"], Tone> = { green: "buy", orange: "warn", red: "sell", gray: "muted" };

/** A button with an explanatory tooltip. */
function Tip({ tip, children }: { tip: string; children: React.ReactElement }) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>{children}</TooltipTrigger>
      <TooltipContent className="max-w-xs">{tip}</TooltipContent>
    </Tooltip>
  );
}

// A co-located strategy's runtime as the OMS last heard it, with the two
// operator actions that only exist for it: approving the venue balances
// (rebaseline) and recording that the old runtime is stopped (takeover).
export function DeploymentPanel({ code, onClose }: { code: string; onClose: () => void }) {
  const qc = useQueryClient();
  const path = `/admin/strategies/${code}/deployments`;
  const query = useQuery<Deployment[]>({ queryKey: [path], queryFn: () => api.get<Deployment[]>(path), refetchInterval: 2000 });
  const d = activeDeployment(query.data);
  const [stopping, setStopping] = useState(false);
  const [evidence, setEvidence] = useState("");
  const refresh = () => qc.invalidateQueries({ queryKey: [path] });

  const rebaseline = () =>
    api
      .post<{ command_id: string }>(`/admin/strategies/${code}/rebaseline`, {})
      .then(() => notifyOk(`${code}: venue balances sent for approval`))
      .catch(notifyError)
      .finally(refresh);
  const verifiedStop = () =>
    d &&
    api
      .post(`${path}/${d.id}/verified-stop`, { shutdown_evidence: evidence, verified_shutdown: true })
      .then(() => {
        notifyOk(`${code}: stop recorded; a new runtime may take over`);
        setStopping(false);
        setEvidence("");
      })
      .catch(notifyError)
      .finally(refresh);

  return (
    <Modal open onClose={onClose} title={`Runtime — ${code}`} className="sm:max-w-lg">
      {!d ? (
        <p className="text-muted-foreground">{query.isLoading ? "Loading…" : "No active deployment. Register one through the admin API."}</p>
      ) : (
        <div className="flex flex-col gap-3">
          <div className="flex flex-wrap gap-1.5">
            {deploymentHealth(d).map((h) =>
              h.detail ? (
                <Tip key={h.label} tip={h.detail}><span><Pill tone={HEALTH_TONE[h.color]}>{h.label}</Pill></span></Tip>
              ) : (
                <Pill key={h.label} tone={HEALTH_TONE[h.color]}>{h.label}</Pill>
              ),
            )}
          </div>
          <dl className="grid grid-cols-[120px_1fr] gap-x-3 gap-y-1">
            <Row label="Deployment" value={d.id} />
            <Row label="Epoch" value={String(d.epoch)} />
            <Row label="Policy" value={`${d.acked_policy_revision ?? "–"} acknowledged of ${d.policy_revision}`} />
            {d.recovery_missing && <Row label="Recovery" value={d.recovery_missing} />}
            {d.inventory_shortfall && <Row label="Shortfall" value={d.inventory_shortfall} />}
          </dl>
          <div className="flex gap-2">
            <Tip tip="After a withdrawal, deposit or manual trade: take the venue's current balances as expected">
              <Button variant="secondary" onClick={rebaseline}>Approve balances</Button>
            </Tip>
            <Tip tip="The old runtime is verifiably stopped; let a replacement take over">
              <Button variant="outline" onClick={() => setStopping(true)}>Record verified stop</Button>
            </Tip>
          </div>
          {stopping && (
            <div className="flex flex-col gap-2">
              <FormRow label="How the stop was verified">
                <Textarea placeholder="e.g. process killed on host X, API key rotated" value={evidence}
                  onChange={(e) => setEvidence(e.currentTarget.value)} className="min-h-16" />
              </FormRow>
              <div className="flex justify-end gap-2">
                <Button variant="outline" onClick={() => setStopping(false)}>Cancel</Button>
                <Button className="bg-warn text-primary-foreground hover:bg-warn/90" disabled={!evidence.trim()} onClick={verifiedStop}>Record stop</Button>
              </div>
            </div>
          )}
        </div>
      )}
    </Modal>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="num text-xs break-all whitespace-pre-wrap">{value}</dd>
    </>
  );
}
