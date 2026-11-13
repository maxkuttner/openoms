import { Alert, Badge, Code, Group, Loader, Stack, Table, Text, Title } from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";

type AuthStatus = {
  configured: boolean;
  issuer: string | null;
  discovery_ok: boolean;
  error: string | null;
  redirect_uri: string | null;
};

export function SignInPage() {
  const status = useQuery<AuthStatus>({ queryKey: ["/admin/auth/status"], queryFn: () => api.get<AuthStatus>("/admin/auth/status") });
  const s = status.data;
  const on = !!s && s.configured && s.discovery_ok;

  return (
    <Stack>
      <Group>
        <Title order={3}>Sign-in</Title>
        {s && <Badge color={on ? "green" : "gray"}>{on ? "on" : "off"}</Badge>}
      </Group>
      {status.isLoading && <Loader />}
      {s && !s.configured && (
        <Alert color="gray" title="Sign-in is not configured">
          Add an <Code>[auth.oidc]</Code> section to the OMS config, set <Code>OMS_OIDC_CLIENT_SECRET</Code>, and
          restart. Register the callback address below with your identity provider.
        </Alert>
      )}
      {s && s.configured && !s.discovery_ok && (
        <Alert color="red" title="Sign-in is configured but not working">
          {s.error ?? "The identity provider could not be reached at startup."} Fix it and restart the OMS.
        </Alert>
      )}
      {s && (
        <Table withTableBorder>
          <Table.Tbody>
            <Table.Tr><Table.Th w={200}>Identity provider</Table.Th><Table.Td>{s.issuer ?? <Text c="dimmed">—</Text>}</Table.Td></Table.Tr>
            <Table.Tr><Table.Th>Callback address</Table.Th><Table.Td>{s.redirect_uri ? <Code>{s.redirect_uri}</Code> : <Text c="dimmed">—</Text>}</Table.Td></Table.Tr>
          </Table.Tbody>
        </Table>
      )}
    </Stack>
  );
}
