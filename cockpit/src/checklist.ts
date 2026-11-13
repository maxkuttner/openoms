// The first-run checklist on the cockpit home page. Pure so it can be tested.

export type SetupStatus = {
  venue_connected: boolean;
  portfolio_exists: boolean;
  sign_in_ok: boolean;
  user_invited: boolean;
  first_order: boolean;
};

export type ChecklistStep = { key: keyof SetupStatus; label: string; to: string; done: boolean };

export function checklistSteps(s: SetupStatus): ChecklistStep[] {
  return [
    { key: "venue_connected", label: "Connect a venue", to: "/broker-connections", done: s.venue_connected },
    { key: "portfolio_exists", label: "Create a portfolio", to: "/portfolios", done: s.portfolio_exists },
    { key: "sign_in_ok", label: "Turn on sign-in", to: "/sign-in", done: s.sign_in_ok },
    { key: "user_invited", label: "Invite a user", to: "/users", done: s.user_invited },
    { key: "first_order", label: "First order shows in the blotter", to: "/blotter", done: s.first_order },
  ];
}

export function checklistVisible(s: SetupStatus): boolean {
  return checklistSteps(s).some((step) => !step.done);
}
