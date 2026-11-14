import { useEffect, useState, type ReactNode } from "react";
import { Alert, Button, Divider, Loader, PasswordInput, Stack, Text, TextInput } from "@mantine/core";
import { authApi, authErrorText, startSso } from "./api";
import { Frame, inputStyles } from "./Frame";
import { Totp, TotpEnroll } from "./Totp";
import { nextStepFromLoginResponse, safeReturnTo, visibleMethods, type AuthMethods, type Step } from "../login";

/**
 * The one sign-in page, rendered by both the cockpit and the trader app.
 * Shows only the methods `/auth/methods` reports enabled. `legacy` renders
 * the extra form for the shared admin password, offered only while the server
 * still accepts it (the cockpit passes it; the trader app doesn't).
 */
export function LoginPage({ home, legacy }: { home: string; legacy?: (methods: AuthMethods) => ReactNode }) {
  const [methods, setMethods] = useState<AuthMethods | null>(null);
  const [failed, setFailed] = useState(false);
  const [step, setStep] = useState<"password" | Exclude<Step, "done">>("password");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const returnTo = safeReturnTo(new URLSearchParams(window.location.search).get("return_to")) ?? home;
  const done = () => { window.location.href = returnTo; };

  useEffect(() => {
    authApi.methods().then(setMethods, () => setFailed(true));
  }, []);

  async function signIn() {
    setBusy(true);
    setError(null);
    try {
      const next = nextStepFromLoginResponse(await authApi.login(email.trim(), password));
      if (next === "done") done();
      else setStep(next);
    } catch (e) {
      setError(authErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  if (step === "totp") return <Frame title="Sign in"><Totp onDone={done} /></Frame>;
  if (step === "totp_enroll") return <Frame title="Sign in"><TotpEnroll onDone={done} /></Frame>;

  if (failed) return <Frame title="Sign in"><Alert color="red">Can't reach the OMS. Reload to try again.</Alert></Frame>;
  if (!methods) return <Frame title="Sign in"><Loader /></Frame>;

  const shown = visibleMethods(methods);
  return (
    <Frame title="Sign in">
      <Stack gap={16}>
        {shown.includes("sso") && (
          <Button variant="default" fullWidth onClick={() => startSso(returnTo)}>{methods.sso_label}</Button>
        )}
        {shown.includes("sso") && shown.includes("local") && <Divider label="or" labelPosition="center" />}
        {shown.includes("local") && (
          <form onSubmit={(e) => { e.preventDefault(); void signIn(); }}>
            <Stack gap={12}>
              <TextInput label="Email" type="email" value={email} onChange={(e) => setEmail(e.currentTarget.value)}
                autoComplete="username" data-autofocus styles={inputStyles} />
              <PasswordInput label="Password" value={password} onChange={(e) => setPassword(e.currentTarget.value)}
                autoComplete="current-password" error={error} styles={inputStyles} />
              <Button type="submit" color="depth" loading={busy} fullWidth>Sign in</Button>
            </Stack>
          </form>
        )}
        {shown.includes("legacy_admin") && legacy?.(methods)}
        {!shown.includes("sso") && !shown.includes("local") && !(shown.includes("legacy_admin") && legacy) && (
          <Text fz={13} c="dimmed">No sign-in method is turned on. An operator has to configure one on the server.</Text>
        )}
      </Stack>
    </Frame>
  );
}
