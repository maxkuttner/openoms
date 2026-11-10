import { useState } from "react";
import { Badge, Button, Group, Modal, Stack, Table, Text, Textarea, Tooltip } from "@mantine/core";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import { notifyError, notifyOk } from "../api/hooks";
import { activeDeployment, deploymentHealth, type Deployment } from "../strategies";

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
    <Modal opened onClose={onClose} title={`Runtime — ${code}`} size="lg" centered>
      {!d ? (
        <Text c="dimmed">{query.isLoading ? "Loading…" : "No active deployment. Register one through the admin API."}</Text>
      ) : (
        <Stack>
          <Group gap="xs">
            {deploymentHealth(d).map((h) => (
              <Tooltip key={h.label} label={h.detail} disabled={!h.detail} multiline w={360}>
                <Badge color={h.color}>{h.label}</Badge>
              </Tooltip>
            ))}
          </Group>
          <Table withRowBorders={false} verticalSpacing={4}>
            <Table.Tbody>
              <Row label="Deployment" value={d.id} />
              <Row label="Epoch" value={String(d.epoch)} />
              <Row label="Policy" value={`${d.acked_policy_revision ?? "–"} acknowledged of ${d.policy_revision}`} />
              {d.recovery_missing && <Row label="Recovery" value={d.recovery_missing} />}
              {d.inventory_shortfall && <Row label="Shortfall" value={d.inventory_shortfall} />}
            </Table.Tbody>
          </Table>
          <Group>
            <Tooltip label="After a withdrawal, deposit or manual trade: take the venue's current balances as expected" multiline w={300}>
              <Button variant="light" onClick={rebaseline}>Approve balances</Button>
            </Tooltip>
            <Tooltip label="The old runtime is verifiably stopped; let a replacement take over" multiline w={300}>
              <Button variant="default" onClick={() => setStopping(true)}>Record verified stop</Button>
            </Tooltip>
          </Group>
          {stopping && (
            <Stack gap="xs">
              <Textarea
                label="How the stop was verified"
                placeholder="e.g. process killed on host X, API key rotated"
                value={evidence}
                onChange={(e) => setEvidence(e.currentTarget.value)}
                autosize
                minRows={2}
              />
              <Group justify="flex-end">
                <Button variant="default" onClick={() => setStopping(false)}>Cancel</Button>
                <Button color="orange" disabled={!evidence.trim()} onClick={verifiedStop}>Record stop</Button>
              </Group>
            </Stack>
          )}
        </Stack>
      )}
    </Modal>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <Table.Tr>
      <Table.Td w={120}><Text size="sm" c="dimmed">{label}</Text></Table.Td>
      <Table.Td><Text size="sm" style={{ whiteSpace: "pre-wrap", wordBreak: "break-all" }}>{value}</Text></Table.Td>
    </Table.Tr>
  );
}
