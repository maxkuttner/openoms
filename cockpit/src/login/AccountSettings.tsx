import { useEffect, useState } from "react";
import { Alert, Button, Code, CopyButton, Group, Loader, Paper, Stack, Table, Text, TextInput, Title } from "@mantine/core";
import { authApi, authErrorText, type OwnKey, type SessionInfo } from "./api";
import { ChangePassword } from "./ChangePassword";
import { inputStyles } from "./Frame";
import { TotpEnroll } from "./Totp";
import { twoStepSection } from "../login";

/** Turn the authenticator off: needs a current 6-digit code. */
function DisableTotp({ onDone }: { onDone: () => void }) {
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit() {
    setBusy(true);
    setError(null);
    try {
      await authApi.totpDisable(code.trim());
      onDone();
    } catch (e) {
      setError(authErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }}>
      <Stack gap={12} maw={360}>
        <Text fz={13}>Two-step sign-in is on. To turn it off, enter a current code from your authenticator app.</Text>
        <TextInput label="Code" value={code} onChange={(e) => setCode(e.currentTarget.value)} autoComplete="one-time-code"
          inputMode="numeric" error={error} styles={inputStyles} />
        <Button type="submit" variant="default" loading={busy}>Turn off two-step sign-in</Button>
      </Stack>
    </form>
  );
}

/** "Two-step sign-in": enrol or remove the authenticator app of a local account. */
function TwoStepSignIn({ me, onChanged }: { me: SessionInfo; onChanged: () => void }) {
  const [enrolling, setEnrolling] = useState(false);
  const section = twoStepSection(me);
  if (section === "hidden") return null;
  return (
    <Paper withBorder p="md">
      <Stack gap={12}>
        <Title order={4}>Two-step sign-in</Title>
        {section === "disable" && <DisableTotp onDone={onChanged} />}
        {section === "enroll" && !enrolling && (
          <Stack gap={12} maw={360}>
            <Text fz={13}>Ask for a code from an authenticator app after your password.</Text>
            <Button onClick={() => setEnrolling(true)}>Turn on two-step sign-in</Button>
          </Stack>
        )}
        {section === "enroll" && enrolling && (
          <Stack maw={420}>
            <TotpEnroll onDone={() => { setEnrolling(false); onChanged(); }} />
          </Stack>
        )}
      </Stack>
    </Paper>
  );
}

/** "API keys": keys that act as you, with your group access (Open WebUI style). */
function OwnKeys() {
  const [keys, setKeys] = useState<OwnKey[] | null>(null);
  const [name, setName] = useState("");
  const [created, setCreated] = useState<OwnKey | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = () => authApi.myKeys().then(setKeys, (e) => setError(authErrorText(e)));
  useEffect(() => { void load(); }, []);
  const act = async (fn: () => Promise<unknown>) => {
    setError(null);
    try { await fn(); await load(); } catch (e) { setError(authErrorText(e)); }
  };
  return (
    <Paper withBorder p="md">
      <Stack gap={12}>
        <Title order={4}>API keys</Title>
        <Text fz={13} c="dimmed">A key acts as you, with your portfolio access. Use it as <Code>Authorization: Bearer key_id.secret</Code>.</Text>
        {error && <Alert color="red">{error}</Alert>}
        {created?.secret && (
          <Alert color="green" title="Copy the secret now: it is shown once">
            <Group gap="xs" wrap="nowrap">
              <Code style={{ overflowWrap: "anywhere" }}>{`${created.key_id}.${created.secret}`}</Code>
              <CopyButton value={`${created.key_id}.${created.secret}`}>
                {({ copied, copy }) => <Button size="xs" variant="light" onClick={copy}>{copied ? "Copied" : "Copy"}</Button>}
              </CopyButton>
            </Group>
          </Alert>
        )}
        <form onSubmit={(e) => { e.preventDefault(); void act(async () => { setCreated(await authApi.createKey(name.trim())); setName(""); }); }}>
          <Group align="flex-end">
            <TextInput label="Name" value={name} onChange={(e) => setName(e.currentTarget.value)} />
            <Button type="submit">Create key</Button>
          </Group>
        </form>
        {keys === null ? <Loader size="sm" /> : keys.length === 0 ? <Text fz={13} c="dimmed">No keys.</Text> : (
          <Table>
            <Table.Tbody>
              {keys.map((k) => (
                <Table.Tr key={k.key_id}>
                  <Table.Td><Code>{k.key_id}</Code></Table.Td>
                  <Table.Td>{k.name ?? "—"}</Table.Td>
                  <Table.Td>{new Date(k.created_at).toLocaleDateString()}</Table.Td>
                  <Table.Td>
                    <Button size="xs" variant="light" color="red"
                      onClick={() => confirm(`Revoke ${k.key_id}?`) && void act(() => authApi.revokeKey(k.key_id))}>Revoke</Button>
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        )}
      </Stack>
    </Paper>
  );
}

/** The account page of both apps: password and two-step sign-in, for local accounts. */
export function AccountSettings() {
  const [me, setMe] = useState<SessionInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const reload = () => authApi.me().then(setMe, (e) => setError(authErrorText(e)));
  useEffect(() => { void reload(); }, []);

  if (error) return <Alert color="red">{error}</Alert>;
  if (!me) return <Loader />;
  if (me.auth_method !== "local") {
    return (
      <Stack>
        <Title order={3}>Account</Title>
        <Text fz={13} c="dimmed">You signed in with SSO: your password and two-step sign-in are managed by your identity provider.</Text>
        <OwnKeys />
      </Stack>
    );
  }
  return (
    <Stack>
      <Title order={3}>Account</Title>
      <Paper withBorder p="md">
        <Stack gap={12} maw={360}>
          <Title order={4}>Password</Title>
          {saved && <Alert color="green">Password changed.</Alert>}
          <ChangePassword key={String(saved)} onNext={() => setSaved(true)} />
        </Stack>
      </Paper>
      <TwoStepSignIn me={me} onChanged={() => void reload()} />
      <OwnKeys />
    </Stack>
  );
}
