// Pure helpers for the Users page.

export type Access = "trade" | "view";

/** Which sign-in method may redeem an invite (`invite.method`). */
export const INVITE_METHOD_OPTIONS = [
  { value: "any", label: "Any sign-in method" },
  { value: "local", label: "Email + password only" },
  { value: "oidc", label: "SSO only" },
];

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
