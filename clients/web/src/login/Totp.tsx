import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { CopyText, FormRow, LinkButton, Notice } from "../components/common";
import { authApi, authErrorText } from "./api";
import { secretFromOtpauth } from "../login";

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
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }} className="flex flex-col gap-3">
      <p>{prompt ?? "Enter the 6-digit code from your authenticator app."}</p>
      <FormRow label={recovery ? "Recovery code" : "Code"} error={error} htmlFor="totp-code">
        <Input id="totp-code" className="num" value={code} onChange={(e) => setCode(e.currentTarget.value)}
          autoComplete="one-time-code" inputMode={recovery ? "text" : "numeric"} autoFocus aria-invalid={!!error} />
      </FormRow>
      <Button type="submit" disabled={busy}>Continue</Button>
      <LinkButton onClick={() => { setRecovery(!recovery); setCode(""); setError(null); }}>
        {recovery ? "Use your authenticator app" : "Use a recovery code"}
      </LinkButton>
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
      <div className="flex flex-col gap-3">
        <p>Save these recovery codes now. Each works once.</p>
        <CopyText label="Recovery codes" value={codes.join("\n")} />
        <Button onClick={onDone}>Continue</Button>
      </div>
    );
  }

  const secret = uri ? secretFromOtpauth(uri) : null;
  return (
    <form onSubmit={(e) => { e.preventDefault(); void confirm(); }} className="flex flex-col gap-3">
      <p>Scan this code with an authenticator app, then enter the 6-digit code.</p>
      {!uri && error && <Notice>{error}</Notice>}
      {uri && (
        <>
          <CopyText label="Setup link (otpauth)" value={uri} />
          {secret && <CopyText label="Or type this key" value={secret} />}
          <FormRow label="Code" error={error} htmlFor="totp-enroll-code">
            <Input id="totp-enroll-code" className="num" value={code} onChange={(e) => setCode(e.currentTarget.value)}
              autoComplete="one-time-code" inputMode="numeric" aria-invalid={!!error} />
          </FormRow>
          <Button type="submit" disabled={busy}>Continue</Button>
        </>
      )}
    </form>
  );
}
