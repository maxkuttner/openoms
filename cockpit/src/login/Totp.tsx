import { useEffect, useRef, useState } from "react";
import { Alert, Anchor, Button, Code, CopyButton, Group, Stack, Text, TextInput } from "@mantine/core";
import { authApi, authErrorText } from "./api";
import { inputStyles } from "./Frame";
import { secretFromOtpauth } from "../login";

function CopyField({ label, value }: { label: string; value: string }) {
  return (
    <Stack gap={4}>
      <Text fz={11} c="dimmed">{label}</Text>
      <Group gap="xs" wrap="nowrap" align="flex-start">
        <Code style={{ overflowWrap: "anywhere", flex: 1 }}>{value}</Code>
        <CopyButton value={value}>
          {({ copied, copy }) => <Button size="xs" variant="light" onClick={copy}>{copied ? "Copied" : "Copy"}</Button>}
        </CopyButton>
      </Group>
    </Stack>
  );
}

/** Check a TOTP code (or a recovery code) on the current session. */
export function Totp({ prompt, onDone, onError }: { prompt?: string; onDone: () => void; onError?: (e: unknown) => void }) {
  const [code, setCode] = useState("");
  const [recovery, setRecovery] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit() {
    setBusy(true);
    setError(null);
    try {
      await authApi.totp(code.trim());
      onDone();
    } catch (e) {
      setError(authErrorText(e));
      onError?.(e);
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }}>
      <Stack gap={12}>
        <Text fz={13}>{prompt ?? "Enter the 6-digit code from your authenticator app."}</Text>
        <TextInput
          label={recovery ? "Recovery code" : "Code"}
          value={code}
          onChange={(e) => setCode(e.currentTarget.value)}
          autoComplete="one-time-code"
          inputMode={recovery ? "text" : "numeric"}
          error={error}
          data-autofocus
          styles={inputStyles}
        />
        <Button type="submit" color="depth" loading={busy} fullWidth>Continue</Button>
        <Anchor component="button" type="button" fz={12} onClick={() => { setRecovery(!recovery); setCode(""); setError(null); }}>
          {recovery ? "Use your authenticator app" : "Use a recovery code"}
        </Anchor>
      </Stack>
    </form>
  );
}

/** Set up TOTP on the current session, then show the recovery codes once. */
export function TotpEnroll({ onDone }: { onDone: () => void }) {
  const [uri, setUri] = useState<string | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Each call replaces the pending secret, so StrictMode's double effect
  // would leave the screen showing a stale one.
  const started = useRef(false);

  useEffect(() => {
    if (started.current) return;
    started.current = true;
    authApi.totpEnroll().then((r) => setUri(r.otpauth_uri), (e) => setError(authErrorText(e)));
  }, []);

  async function confirm() {
    setBusy(true);
    setError(null);
    try {
      setCodes((await authApi.totpConfirm(code.trim())).recovery_codes);
    } catch (e) {
      setError(authErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  if (codes) {
    return (
      <Stack gap={12}>
        <Text fz={13}>Save these recovery codes now. Each works once.</Text>
        <CopyField label="Recovery codes" value={codes.join("\n")} />
        <Button color="depth" fullWidth onClick={onDone}>Continue</Button>
      </Stack>
    );
  }

  const secret = uri ? secretFromOtpauth(uri) : null;
  return (
    <form onSubmit={(e) => { e.preventDefault(); void confirm(); }}>
      <Stack gap={12}>
        <Text fz={13}>Scan this code with an authenticator app, then enter the 6-digit code.</Text>
        {!uri && error && <Alert color="red">{error}</Alert>}
        {uri && (
          <>
            <CopyField label="Setup link (otpauth)" value={uri} />
            {secret && <CopyField label="Or type this key" value={secret} />}
            <TextInput
              label="Code"
              value={code}
              onChange={(e) => setCode(e.currentTarget.value)}
              autoComplete="one-time-code"
              inputMode="numeric"
              error={error}
              styles={inputStyles}
            />
            <Button type="submit" color="depth" loading={busy} fullWidth>Continue</Button>
          </>
        )}
      </Stack>
    </form>
  );
}
