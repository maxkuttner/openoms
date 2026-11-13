import { useState } from "react";
import { Button, Divider, PasswordInput, Stack, Text } from "@mantine/core";
import { consoleAccess, setLegacyPassword } from "../api/client";
import { LoginPage as SharedLoginPage } from "../login/LoginPage";
import { inputStyles } from "../login/Frame";
import { COCKPIT_HOME } from "../login/api";

/**
 * The deprecated shared admin password. Offered only while the server still
 * accepts it (`/auth/methods` `legacy_admin`), and only to create the first
 * named admin. Held in memory for this page, never stored.
 */
function LegacyAdminForm({ onSignedIn }: { onSignedIn: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit() {
    setBusy(true);
    setError(null);
    setLegacyPassword(password);
    const access = await consoleAccess().catch(() => "signed_out" as const);
    setBusy(false);
    if (access === "admin") onSignedIn();
    else {
      setLegacyPassword(null);
      setError("Wrong password.");
    }
  }

  return (
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }}>
      <Stack gap={12}>
        <Divider label="First admin" labelPosition="center" />
        <Text fz={12} c="dimmed">
          No named admin yet? Use the shared admin password once to create one.
        </Text>
        <PasswordInput label="Shared admin password" value={password} onChange={(e) => setPassword(e.currentTarget.value)}
          error={error} autoComplete="off" styles={inputStyles} />
        <Button type="submit" variant="default" loading={busy} fullWidth>Create the first admin</Button>
      </Stack>
    </form>
  );
}

/** The cockpit's /login: the shared sign-in page plus the first-admin form while it applies. */
export function LoginPage({ onLegacySignedIn }: { onLegacySignedIn: () => void }) {
  return <SharedLoginPage home={COCKPIT_HOME} legacy={() => <LegacyAdminForm onSignedIn={onLegacySignedIn} />} />;
}
