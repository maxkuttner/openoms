import { useState } from "react";
import { Button, PasswordInput, Stack, Text } from "@mantine/core";
import { authApi, authErrorText } from "./api";
import { inputStyles } from "./Frame";
import { nextStepFromLoginResponse, type Step } from "../login";

const MIN_LENGTH = 15;

/** Claim an invite with a new password. */
export function Enroll({ token, onNext }: { token: string; onNext: (step: Step) => void }) {
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
      onNext(nextStepFromLoginResponse(await authApi.enroll(token, password)));
    } catch (e) {
      setError(authErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }}>
      <Stack gap={12}>
        <Text fz={13}>Choose a password (at least 15 characters).</Text>
        <PasswordInput label="Password" value={password} onChange={(e) => setPassword(e.currentTarget.value)}
          autoComplete="new-password" data-autofocus styles={inputStyles} />
        <PasswordInput label="Password again" value={again} onChange={(e) => setAgain(e.currentTarget.value)}
          autoComplete="new-password" error={error} styles={inputStyles} />
        <Button type="submit" color="depth" loading={busy} fullWidth>Set password</Button>
      </Stack>
    </form>
  );
}
