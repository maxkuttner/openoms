import { useEffect, useState } from "react";
import { Alert, Button, Loader, Paper, Stack, Text, TextInput, Title } from "@mantine/core";
import { authApi, authErrorText, type SessionInfo } from "./api";
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
    </Stack>
  );
}
