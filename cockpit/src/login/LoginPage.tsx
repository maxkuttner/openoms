import { useEffect, useState } from "react";
import { Alert, Anchor, Button, Divider, Loader, PasswordInput, Stack, Text, TextInput } from "@mantine/core";
import { authApi, authErrorText, startSso } from "./api";
import { Frame, inputStyles } from "./Frame";
import { Totp, TotpEnroll } from "./Totp";
import { ForcedPasswordChange } from "./ChangePassword";
import { loginMode, nextStepFromLoginResponse, safeReturnTo, visibleMethods, type AuthMethods, type Step } from "../login";

const MIN_LENGTH = 15;

/** Name, email and password: the sign-up form, and on a fresh install the admin's. */
function SignupForm({ submitLabel, onNext }: { submitLabel: string; onNext: (step: Step) => void }) {
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
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
      onNext(nextStepFromLoginResponse(await authApi.signup(name.trim(), email.trim(), password)));
    } catch (e) {
      setError(authErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }}>
      <Stack gap={12}>
        <TextInput label="Name" value={name} onChange={(e) => setName(e.currentTarget.value)} autoComplete="name"
          data-autofocus styles={inputStyles} />
        <TextInput label="Email" type="email" value={email} onChange={(e) => setEmail(e.currentTarget.value)}
          autoComplete="username" styles={inputStyles} />
        <PasswordInput label="Password (at least 15 characters)" value={password} onChange={(e) => setPassword(e.currentTarget.value)}
          autoComplete="new-password" styles={inputStyles} />
        <PasswordInput label="Password again" value={again} onChange={(e) => setAgain(e.currentTarget.value)}
          autoComplete="new-password" error={error} styles={inputStyles} />
        <Button type="submit" color="depth" loading={busy} fullWidth>{submitLabel}</Button>
      </Stack>
    </form>
  );
}

/**
 * The one sign-in page, rendered by both the cockpit and the trader app.
 * Shows only the methods `/auth/methods` reports enabled. On a fresh install
 * (no admin yet) it offers "Create the admin account" instead; otherwise a
 * "Sign up" link when self-signup is on.
 */
export function LoginPage({ home }: { home: string }) {
  const [methods, setMethods] = useState<AuthMethods | null>(null);
  const [failed, setFailed] = useState(false);
  const [step, setStep] = useState<"password" | "signup" | Exclude<Step, "done">>("password");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const returnTo = safeReturnTo(new URLSearchParams(window.location.search).get("return_to")) ?? home;
  const done = () => { window.location.href = returnTo; };
  const next = (s: Step) => (s === "done" ? done() : setStep(s));

  useEffect(() => {
    authApi.methods().then(setMethods, () => setFailed(true));
  }, []);

  async function signIn() {
    setBusy(true);
    setError(null);
    try {
      next(nextStepFromLoginResponse(await authApi.login(email.trim(), password)));
    } catch (e) {
      setError(authErrorText(e));
    } finally {
      setBusy(false);
    }
  }

  if (step === "change_password") return <ForcedPasswordChange onDone={done} />;
  if (step === "totp") return <Frame title="Sign in"><Totp onDone={done} /></Frame>;
  if (step === "totp_enroll") return <Frame title="Sign in"><TotpEnroll onDone={done} /></Frame>;

  if (failed) return <Frame title="Sign in"><Alert color="red">Can't reach the OMS. Reload to try again.</Alert></Frame>;
  if (!methods) return <Frame title="Sign in"><Loader /></Frame>;

  if (loginMode(methods) === "create_admin") {
    return (
      <Frame title="Create the admin account">
        <Stack gap={12}>
          <Text fz={13}>This OMS has no accounts yet. The account you create now is the administrator.</Text>
          <SignupForm submitLabel="Create the admin account" onNext={next} />
        </Stack>
      </Frame>
    );
  }
  if (step === "signup") {
    return (
      <Frame title="Sign up">
        <Stack gap={12}>
          <SignupForm submitLabel="Sign up" onNext={next} />
          <Anchor component="button" type="button" fz={12} onClick={() => setStep("password")}>I already have an account</Anchor>
        </Stack>
      </Frame>
    );
  }

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
        {shown.includes("local") && methods.signup && (
          <Anchor component="button" type="button" fz={12} onClick={() => setStep("signup")}>No account? Sign up</Anchor>
        )}
        {shown.length === 0 && (
          <Text fz={13} c="dimmed">No sign-in method is turned on. An operator has to configure one on the server.</Text>
        )}
      </Stack>
    </Frame>
  );
}
