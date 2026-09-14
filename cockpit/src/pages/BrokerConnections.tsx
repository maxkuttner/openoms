import { useState } from "react";
import { Button, Drawer, Stack } from "@mantine/core";
import { CrudResource } from "../components/CrudResource";
import { CredentialsPanel } from "../components/CredentialsPanel";
import { StreamHealthStrip } from "../components/StreamHealthStrip";
import type { BrokerConnection } from "../api/types";

const STATUS = [
  { value: "ACTIVE", label: "ACTIVE" },
  { value: "INACTIVE", label: "INACTIVE" },
];

export function BrokerConnectionsPage() {
  const [selected, setSelected] = useState<BrokerConnection | null>(null);

  return (
    <Stack gap="lg">
      <StreamHealthStrip
        kind="execution"
        title="Live execution streams"
        emptyText="No execution streams — no broker credentials configured in this process."
      />
      <CrudResource
      title="Broker connections"
      path="/admin/broker-connections"
      idKey="code"
      editable
      rowAction={(row: BrokerConnection) => (
        <Button size="xs" variant="light" onClick={() => setSelected(row)}>
          Credentials
        </Button>
      )}
      columns={[
        { key: "code", label: "Code" },
        { key: "broker_code", label: "Broker" },
        { key: "environment", label: "Environment" },
        { key: "status", label: "Status" },
      ]}
      fields={[
        { name: "code", label: "Code", required: true, inEdit: false },
        { name: "broker_code", label: "Broker code", required: true },
        {
          name: "environment",
          label: "Environment",
          type: "select",
          required: true,
          options: [
            { value: "PAPER", label: "PAPER" },
            { value: "LIVE", label: "LIVE" },
          ],
        },
        { name: "status", label: "Status", type: "select", required: true, options: STATUS },
      ]}
      />

      <Drawer
        opened={!!selected}
        onClose={() => setSelected(null)}
        title={selected ? `Credentials — ${selected.code}` : undefined}
        position="right"
        size="md"
      >
        {selected && <CredentialsPanel kind="broker" code={selected.code} providerCode={selected.broker_code} />}
      </Drawer>
    </Stack>
  );
}
