import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableRow } from "@/components/ui/table";
import { ConfirmButton } from "../components/ConfirmButton";
import { CopyText, FormRow, Notice, Section, Spinner } from "../components/common";
import { authApi, authErrorText, type OwnKey, type SessionInfo } from "./api";
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
    <Section title="API keys">
      <p className="text-muted-foreground">
        A key acts as you, with your portfolio access. Use it as <code className="num">Authorization: Bearer key_id.secret</code>.
      </p>
      {error && <Notice>{error}</Notice>}
      {created?.secret && (
        <Notice tone="success" title="Copy the secret now: it is shown once">
          <CopyText value={`${created.key_id}.${created.secret}`} />
        </Notice>
      )}
      <form className="flex items-end gap-2"
        onSubmit={(e) => { e.preventDefault(); void act(async () => { setCreated(await authApi.createKey(name.trim())); setName(""); }); }}>
        <FormRow label="Name" htmlFor="key-name">
          <Input id="key-name" value={name} onChange={(e) => setName(e.currentTarget.value)} />
        </FormRow>
        <Button type="submit">Create key</Button>
      </form>
      {keys === null ? <Spinner /> : keys.length === 0 ? <p className="text-muted-foreground">No keys.</p> : (
        <Table>
          <TableBody>
            {keys.map((k) => (
              <TableRow key={k.key_id}>
                <TableCell><code className="num">{k.key_id}</code></TableCell>
                <TableCell>{k.name ?? "—"}</TableCell>
                <TableCell className="num">{new Date(k.created_at).toLocaleDateString()}</TableCell>
                <TableCell className="text-right">
                  <ConfirmButton size="xs" variant="destructive" title={`Revoke ${k.key_id}?`} confirmLabel="Revoke"
                    onConfirm={() => act(() => authApi.revokeKey(k.key_id))}>Revoke</ConfirmButton>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </Section>
  );
}

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
      <OwnKeys />
    </div>
  );
}
