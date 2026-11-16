import { useState } from "react";
import { Button, PasswordInput, Stack, Text } from "@mantine/core";
import { AuthError, authApi, authErrorText } from "./api";
import { Frame, inputStyles } from "./Frame";
import { nextStepFromLoginResponse, PENDING_MESSAGE, type Step } from "../login";

const MIN_LENGTH = 15;

/** Change the signed-in local account's password; `onNext` gets what comes after. */
export function ChangePassword({ prompt, onNext }: { prompt?: string; onNext: (step: Step) => void }) {
  const [current, setCurrent] = useState("");
  const [password, setPassword] = useState("");
  const [again, setAgain] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit() {
    if (password.length < MIN_LENGTH) return setError(`Use at least ${MIN_LENGTH} characters.`);
    if (password !== again) return setError("The passwords don't match.");
    setBusy(true);
    setError(null);
    try {
      onNext(nextStepFromLoginResponse(await authApi.changePassword(current, password)));
    } catch (e) {
      setError(e instanceof AuthError && e.code === "invalid_credentials" ? "Wrong current password." : authErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }}>
      <Stack gap={12}>
        <Text fz={13}>{prompt ?? "Choose a new password (at least 15 characters)."}</Text>
        <PasswordInput label="Current password" value={current} onChange={(e) => setCurrent(e.currentTarget.value)}
          autoComplete="current-password" data-autofocus styles={inputStyles} />
        <PasswordInput label="New password" value={password} onChange={(e) => setPassword(e.currentTarget.value)}
          autoComplete="new-password" styles={inputStyles} />
        <PasswordInput label="New password again" value={again} onChange={(e) => setAgain(e.currentTarget.value)}
          autoComplete="new-password" error={error} styles={inputStyles} />
        <Button type="submit" color="depth" loading={busy} fullWidth>Change password</Button>
      </Stack>
    </form>
  );
}

/** What a pending account sees, in both apps, until an admin approves it. */
export function PendingScreen() {
  return (
    <Frame title="Account pending">
      <Stack gap={12}>
        <Text fz={13}>{PENDING_MESSAGE}</Text>
        <Button variant="default" fullWidth
          onClick={() => { void authApi.logout().catch(() => undefined).then(() => window.location.reload()); }}>
          Sign out
        </Button>
      </Stack>
    </Frame>
  );
}
