import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ApiKeysBox, type ApiKeysSource } from "../components/ApiKeysBox";
import { FormRow, Notice, Section, Spinner } from "../components/common";
import { authApi, authErrorText, type SessionInfo } from "./api";
import { ChangePassword } from "./ChangePassword";
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
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }} className="flex max-w-sm flex-col gap-3">
      <p>Two-step sign-in is on. To turn it off, enter a current code from your authenticator app.</p>
      <FormRow label="Code" error={error} htmlFor="dt-code">
        <Input id="dt-code" className="num" value={code} onChange={(e) => setCode(e.currentTarget.value)}
          autoComplete="one-time-code" inputMode="numeric" aria-invalid={!!error} />
      </FormRow>
      <Button type="submit" variant="outline" disabled={busy}>Turn off two-step sign-in</Button>
    </form>
  );
}

/** "Two-step sign-in": enrol or remove the authenticator app of a local account. */
function TwoStepSignIn({ me, onChanged }: { me: SessionInfo; onChanged: () => void }) {
  const [enrolling, setEnrolling] = useState(false);
  const section = twoStepSection(me);
  if (section === "hidden") return null;
  return (
    <Section title="Two-step sign-in">
      {section === "disable" && <DisableTotp onDone={onChanged} />}
      {section === "enroll" && !enrolling && (
        <div className="flex max-w-sm flex-col gap-3">
          <p>Ask for a code from an authenticator app after your password.</p>
          <Button className="self-start" onClick={() => setEnrolling(true)}>Turn on two-step sign-in</Button>
        </div>
      )}
      {section === "enroll" && enrolling && (
        <div className="max-w-md">
          <TotpEnroll onDone={() => { setEnrolling(false); onChanged(); }} />
        </div>
      )}
    </Section>
  );
}

/** Your own keys, through the session's `/auth/keys`. */
const ownKeys: ApiKeysSource = {
  list: authApi.myKeys,
  create: authApi.createKey,
  revoke: authApi.revokeKey,
  errorText: authErrorText,
};

/** The account page: password and two-step sign-in for local accounts, and API keys. */
export function AccountSettings() {
  const [me, setMe] = useState<SessionInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const reload = () => authApi.me().then(setMe, (e) => setError(authErrorText(e)));
  useEffect(() => { void reload(); }, []);

  if (error) return <Notice>{error}</Notice>;
  if (!me) return <Spinner />;
  return (
    <div className="flex max-w-3xl flex-col gap-4">
      {me.auth_method !== "local" ? (
        <p className="text-muted-foreground">You signed in with SSO: your password and two-step sign-in are managed by your identity provider.</p>
      ) : (
        <>
          <Section title="Password">
            <div className="flex max-w-sm flex-col gap-3">
              {saved && <Notice tone="success">Password changed.</Notice>}
              <ChangePassword key={String(saved)} onNext={() => setSaved(true)} />
            </div>
          </Section>
          <TwoStepSignIn me={me} onChanged={() => void reload()} />
        </>
      )}
      <ApiKeysBox
        source={ownKeys}
        intro={<>A key acts as you, with your portfolio access. Use it as <code className="num">Authorization: Bearer key_id.secret</code>.</>}
      />
    </div>
  );
}
