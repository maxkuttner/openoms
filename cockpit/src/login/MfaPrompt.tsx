import { useEffect, useState } from "react";
import { Button, Modal, Stack, Text } from "@mantine/core";
import { AuthError, sessionAuthMethod, startSso } from "./api";
import { Totp } from "./Totp";
import { mfaPromptText, mfaStepFor, type MfaStep } from "../login";

type Register = (prompt: () => Promise<boolean>) => () => void;

/**
 * The step behind every 403 mfa_required. A local session confirms with its
 * authenticator and the waiting request is retried; an SSO session has to
 * sign in again (a full-page redirect, so the request is dropped).
 */
export function MfaPrompt({ register }: { register: Register }) {
  const [pending, setPending] = useState<((ok: boolean) => void) | null>(null);
  const [step, setStep] = useState<MfaStep | null>(null);

  useEffect(
    () =>
      register(
        () =>
          new Promise<boolean>((resolve) => {
            setStep(null);
            setPending(() => resolve);
            void sessionAuthMethod().then((m) => setStep(mfaStepFor(m)));
          }),
      ),
    [register],
  );

  const finish = (ok: boolean) => {
    pending?.(ok);
    setPending(null);
  };
  const here = window.location.pathname + window.location.search;

  return (
    <Modal opened={!!pending} onClose={() => finish(false)} title="Confirm" centered>
      {step === "totp" && (
        <Totp
          prompt={mfaPromptText("totp")}
          onDone={() => finish(true)}
          onError={(e) => { if (e instanceof AuthError && mfaStepFor("local", e.code) === "sso") setStep("sso"); }}
        />
      )}
      {step === "sso" && (
        <Stack gap={12}>
          <Text fz={13}>{mfaPromptText("sso")}</Text>
          <Button color="depth" fullWidth onClick={() => startSso(here)}>Sign in with SSO</Button>
        </Stack>
      )}
    </Modal>
  );
}
