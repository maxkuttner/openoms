import { useEffect, useState } from "react";
import { Alert, Button, Loader, Stack, Text } from "@mantine/core";
import { authApi, COCKPIT_HOME, TRADE_HOME, type InvitePreview as Preview } from "./api";
import { Frame } from "./Frame";
import { Enroll } from "./Enroll";
import { Totp, TotpEnroll } from "./Totp";
import { inviteSummary, inviteTokenFromHash, type Step } from "../login";

// The token is kept in state and dropped from the address bar once the
// preview has answered, so it is not left in history or on screen (a failed
// call leaves it there for a reload).
function readToken(): string | null {
  return inviteTokenFromHash(window.location.hash);
}

function clearToken() {
  window.history.replaceState(null, "", window.location.pathname + window.location.search);
}

/** Where an invite link lands: what the invite grants, then the enabled ways to accept it. */
export function InvitePreview() {
  const [token] = useState(readToken);
  const [preview, setPreview] = useState<Preview | null>(null);
  const [failed, setFailed] = useState(false);
  const [step, setStep] = useState<"choose" | "password" | Exclude<Step, "done">>("choose");

  useEffect(() => {
    if (token) {
      authApi.preview(token).then((p) => { clearToken(); setPreview(p); }, () => setFailed(true));
    }
  }, [token]);

  // A named admin's invite (no role) opens the cockpit; everyone else the trader app.
  const home = preview?.role === null ? COCKPIT_HOME : TRADE_HOME;
  const done = () => { window.location.href = home; };

  if (!token || (preview && !preview.valid)) {
    return (
      <Frame title="Invite">
        <Alert color="amber">This invite link isn't valid any more. Ask for a new one.</Alert>
      </Frame>
    );
  }
  if (failed) {
    return <Frame title="Invite"><Alert color="red">Can't reach the OMS. Reload to try again.</Alert></Frame>;
  }
  if (!preview) return <Frame title="Invite"><Loader /></Frame>;

  if (step === "password") return <Frame title="Set a password"><Enroll token={token} onNext={(s) => (s === "done" ? done() : setStep(s))} /></Frame>;
  if (step === "totp_enroll") return <Frame title="Authenticator"><TotpEnroll onDone={done} /></Frame>;
  if (step === "totp") return <Frame title="Authenticator"><Totp onDone={done} /></Frame>;

  return (
    <Frame title="Invite">
      <Stack gap={12}>
        <Text fz={13}>{inviteSummary(preview)}</Text>
        {preview.email_hint && <Text fz={12} c="dimmed">For {preview.email_hint}</Text>}
        {preview.methods.includes("local") && (
          <Button color="depth" fullWidth onClick={() => setStep("password")}>Set a password</Button>
        )}
        {preview.methods.includes("oidc") && (
          <Button variant="default" fullWidth onClick={() => { window.location.href = `/auth/invite/${encodeURIComponent(token)}`; }}>
            Sign in with SSO
          </Button>
        )}
        {preview.methods.length === 0 && <Alert color="amber">No sign-in method is turned on. Ask the OMS admin.</Alert>}
      </Stack>
    </Frame>
  );
}
