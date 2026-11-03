import { useState } from "react";
import { Button, Drawer, Stack } from "@mantine/core";
import { CrudResource } from "../components/CrudResource";
import { statusOptions } from "../lifecycle";
import { api } from "../api/client";
import { useList } from "../api/hooks";
import { autoCode, environmentOptions, venueOptions, type Venue } from "../venues";
import { CredentialsPanel } from "../components/CredentialsPanel";
import { StreamHealthStrip } from "../components/StreamHealthStrip";
import { UniverseStatus } from "../components/UniverseStatus";
import type { BrokerConnection } from "../api/types";

const STATUS = statusOptions("brokerConnection");

export function BrokerConnectionsPage() {
  const [selected, setSelected] = useState<BrokerConnection | null>(null);
  const venues = useList<Venue>("/admin/venues").data ?? [];
  const venueOf = (code: string) => venues.find((v) => v.code === code);

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
      lifecycle={{ resource: "brokerConnection", afterChange: () => api.post("/admin/connections/reload", {}) }}
      derive={(previous, next) => ({ code: autoCode(previous as any, next as any) })}
      // Credentials are the next step for a new connection, so open them.
      onCreated={(row: BrokerConnection) => setSelected(row)}
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
        {
          key: "universe",
          label: "Instrument universe",
          render: (row: BrokerConnection) => <UniverseStatus code={row.code} />,
        },
      ]}
      fields={[
        {
          name: "broker_code",
          label: "Venue",
          type: "select",
          required: true,
          inEdit: false,
          options: venueOptions(venues),
        },
        {
          name: "environment",
          label: "Environment",
          type: "select",
          required: true,
          options: (values) => environmentOptions(venueOf(values.broker_code)),
        },
        {
          name: "code",
          label: "Code",
          required: true,
          inEdit: false,
          description: () => "Your name for this connection; accounts and orders refer to it.",
        },
        { name: "status", label: "Status", type: "select", required: true, options: STATUS, default: "ACTIVE" },
      ]}
      />

      <Drawer
        opened={!!selected}
        onClose={() => setSelected(null)}
        title={selected ? `Credentials — ${selected.code}` : undefined}
        position="right"
        size="md"
      >
        {selected && (
          <CredentialsPanel
            kind="broker"
            code={selected.code}
            providerCode={selected.broker_code}
            environment={selected.environment}
          />
        )}
      </Drawer>
    </Stack>
  );
}
