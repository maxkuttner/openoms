import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FormRow } from "../components/common";
import { AuthError, authApi, authErrorText } from "./api";
import { Frame } from "./Frame";
import { nextStepFromLoginResponse, PENDING_MESSAGE, type Step } from "../login";
import { Totp, TotpEnroll } from "./Totp";

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
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }} className="flex flex-col gap-3">
      <p>{prompt ?? "Choose a new password (at least 15 characters)."}</p>
      <FormRow label="Current password" htmlFor="cp-current">
        <Input id="cp-current" type="password" value={current} onChange={(e) => setCurrent(e.currentTarget.value)}
          autoComplete="current-password" autoFocus />
      </FormRow>
      <FormRow label="New password" htmlFor="cp-new">
        <Input id="cp-new" type="password" value={password} onChange={(e) => setPassword(e.currentTarget.value)} autoComplete="new-password" />
      </FormRow>
      <FormRow label="New password again" error={error} htmlFor="cp-again">
        <Input id="cp-again" type="password" value={again} onChange={(e) => setAgain(e.currentTarget.value)}
          autoComplete="new-password" aria-invalid={!!error} />
      </FormRow>
      <Button type="submit" disabled={busy}>Change password</Button>
    </form>
  );
}

/** What a pending account sees until an admin approves it. */
export function PendingScreen() {
  return (
    <Frame title="Account pending">
      <div className="flex flex-col gap-3">
        <p>{PENDING_MESSAGE}</p>
        <Button variant="outline"
          onClick={() => { void authApi.logout().catch(() => undefined).then(() => window.location.reload()); }}>
          Sign out
        </Button>
      </div>
    </Frame>
  );
}

/**
 * The full-page "Choose a new password" screen for a session on a password
 * the host CLI generated. Runs any second-factor step the change leads to,
 * then calls `onDone`.
 */
export function ForcedPasswordChange({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState<Step>("change_password");
  if (step === "totp") return <Frame title="Sign in"><Totp onDone={onDone} /></Frame>;
  if (step === "totp_enroll") return <Frame title="Authenticator"><TotpEnroll onDone={onDone} /></Frame>;
  return (
    <Frame title="Choose a new password">
      <ChangePassword
        prompt="This password was generated for you and must be changed. Choose your own (at least 15 characters) to continue."
        onNext={(s) => (s === "done" ? onDone() : setStep(s))}
      />
    </Frame>
  );
}
