import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { FormRow, LinkButton, Notice, Spinner } from "../components/common";
import { authApi, authErrorText, startSso } from "./api";
import { Frame } from "./Frame";
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
    <form onSubmit={(e) => { e.preventDefault(); void submit(); }} className="flex flex-col gap-3">
      <FormRow label="Name" htmlFor="su-name">
        <Input id="su-name" value={name} onChange={(e) => setName(e.currentTarget.value)} autoComplete="name" autoFocus />
      </FormRow>
      <FormRow label="Email" htmlFor="su-email">
        <Input id="su-email" type="email" value={email} onChange={(e) => setEmail(e.currentTarget.value)} autoComplete="username" />
      </FormRow>
      <FormRow label="Password (at least 15 characters)" htmlFor="su-pw">
        <Input id="su-pw" type="password" value={password} onChange={(e) => setPassword(e.currentTarget.value)} autoComplete="new-password" />
      </FormRow>
      <FormRow label="Password again" error={error} htmlFor="su-again">
        <Input id="su-again" type="password" value={again} onChange={(e) => setAgain(e.currentTarget.value)}
          autoComplete="new-password" aria-invalid={!!error} />
      </FormRow>
      <Button type="submit" disabled={busy}>{submitLabel}</Button>
    </form>
  );
}

/**
 * The one sign-in page. Shows only the methods `/auth/methods` reports
 * enabled. On a fresh install (no admin yet) it offers "Create the admin
 * account" instead; otherwise a "Sign up" link when self-signup is on.
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

  if (failed) return <Frame title="Sign in"><Notice>Can't reach the OMS. Reload to try again.</Notice></Frame>;
  if (!methods) return <Frame title="Sign in"><Spinner /></Frame>;

  if (loginMode(methods) === "create_admin") {
    return (
      <Frame title="Create the admin account">
        <div className="flex flex-col gap-3">
          <p>This OMS has no accounts yet. The account you create now is the administrator.</p>
          <SignupForm submitLabel="Create the admin account" onNext={next} />
        </div>
      </Frame>
    );
  }
  if (step === "signup") {
    return (
      <Frame title="Sign up">
        <div className="flex flex-col gap-3">
          <SignupForm submitLabel="Sign up" onNext={next} />
          <LinkButton onClick={() => setStep("password")}>I already have an account</LinkButton>
        </div>
      </Frame>
    );
  }

  const shown = visibleMethods(methods);
  return (
    <Frame title="Sign in">
      <div className="flex flex-col gap-4">
        {shown.includes("sso") && (
          <Button variant="outline" onClick={() => startSso(returnTo)}>{methods.sso_label}</Button>
        )}
        {shown.includes("sso") && shown.includes("local") && (
          <div className="flex items-center gap-3 text-xs text-muted-foreground"><Separator className="flex-1" />or<Separator className="flex-1" /></div>
        )}
        {shown.includes("local") && (
          <form onSubmit={(e) => { e.preventDefault(); void signIn(); }} className="flex flex-col gap-3">
            <FormRow label="Email" htmlFor="li-email">
              <Input id="li-email" type="email" value={email} onChange={(e) => setEmail(e.currentTarget.value)} autoComplete="username" autoFocus />
            </FormRow>
            <FormRow label="Password" error={error} htmlFor="li-pw">
              <Input id="li-pw" type="password" value={password} onChange={(e) => setPassword(e.currentTarget.value)}
                autoComplete="current-password" aria-invalid={!!error} />
            </FormRow>
            <Button type="submit" disabled={busy}>Sign in</Button>
          </form>
        )}
        {shown.includes("local") && methods.signup && (
          <LinkButton onClick={() => setStep("signup")}>No account? Sign up</LinkButton>
        )}
        {shown.length === 0 && (
          <p className="text-muted-foreground">No sign-in method is turned on. An operator has to configure one on the server.</p>
        )}
      </div>
    </Frame>
  );
}
