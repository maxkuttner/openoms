// Pure helpers for the Users page.

export type Access = "trade" | "view";

export const ROLE_OPTIONS = [
  { value: "pm", label: "PM" },
  { value: "trader", label: "Trader" },
  { value: "viewer", label: "Viewer" },
];

export function roleLabel(role: string | null): string {
  return ROLE_OPTIONS.find((r) => r.value === role)?.label ?? role ?? "—";
}

/** Picker state (portfolio id -> access, absent = none) to the API body. */
export function accessBody(picked: Record<string, Access>): { portfolio_id: string; can_trade: boolean }[] {
  return Object.entries(picked).map(([portfolio_id, a]) => ({ portfolio_id, can_trade: a === "trade" }));
}

export function inviteNotice(email: string, expiresAt: string): string {
  return `Send this link to ${email}. It expires on ${new Date(expiresAt).toLocaleDateString()}.`;
}

export const SIGN_IN_WARNING = "Invitees can't sign in until sign-in is turned on.";

export type AccessReason = "no_invite" | "expired" | "revoked" | "email_mismatch" | "email_unverified";

/** Text for the trader app's access page; unknown reasons read as no invite. */
export function accessMessage(reason: string | null, hint: string | null): string {
  switch (reason) {
    case "expired":
      return "This invite expired. Ask for a new one.";
    case "revoked":
      return "This invite was revoked.";
    case "email_mismatch":
      return `This invite is for ${hint ?? "another address"}. Sign in with that address, or ask for a new invite.`;
    case "email_unverified":
      return "Your sign-in provider didn't confirm your email address. Use another sign-in method or ask for a new invite.";
    default:
      return "Access not granted. Ask the OMS admin for an invite.";
  }
}
