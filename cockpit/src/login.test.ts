import { test } from "node:test";
import assert from "node:assert/strict";
import {
  accessMessage, inviteSummary, inviteTokenFromHash, loginHref, nextStepFromLoginResponse, safeReturnTo,
  secretFromOtpauth, visibleMethods,
} from "./login.ts";

test("only enabled sign-in methods are shown", () => {
  const off = { local: false, sso: false, sso_label: "Sign in with SSO", legacy_admin: false };
  assert.deepEqual(visibleMethods(off), []);
  assert.deepEqual(visibleMethods({ ...off, sso: true }), ["sso"]);
  assert.deepEqual(visibleMethods({ ...off, local: true }), ["local"]);
  assert.deepEqual(visibleMethods({ ...off, local: true, sso: true }), ["sso", "local"]);
  assert.deepEqual(visibleMethods({ ...off, legacy_admin: true }), ["legacy_admin"]);
});

test("the login response names the next step", () => {
  assert.equal(nextStepFromLoginResponse({ next: "totp" }), "totp");
  assert.equal(nextStepFromLoginResponse({ next: "totp_enroll" }), "totp_enroll");
  assert.equal(nextStepFromLoginResponse({ mfa_verified: true }), "done");
  assert.equal(nextStepFromLoginResponse(undefined), "done");
  assert.equal(nextStepFromLoginResponse({ next: "something_else" }), "done");
});

test("access reasons read as plain text", () => {
  assert.equal(accessMessage("pending_approval", null), "Your sign-in is waiting for an administrator to approve it.");
  assert.equal(accessMessage("expired", null), "This invite expired. Ask for a new one.");
  assert.equal(accessMessage("revoked", null), "This invite was revoked.");
  assert.equal(
    accessMessage("email_mismatch", "j…@fundco.com"),
    "This invite is for j…@fundco.com. Sign in with that address, or ask for a new invite.",
  );
  assert.equal(accessMessage(null, null), "Access not granted. Ask the OMS admin for an invite.");
});

test("an invite reads as role and portfolios", () => {
  assert.equal(inviteSummary({ role: "trader", portfolios: ["ALPHA", "BETA"] }), "You're invited as Trader on ALPHA, BETA.");
  assert.equal(inviteSummary({ role: "pm", portfolios: ["ALPHA"] }), "You're invited as PM on ALPHA.");
  assert.equal(inviteSummary({ role: "viewer", portfolios: [] }), "You're invited as Viewer.");
  assert.equal(inviteSummary({ role: null, portfolios: [] }), "You're invited as Admin.");
});

test("the TOTP secret is read from the otpauth URI", () => {
  assert.equal(secretFromOtpauth("otpauth://totp/OMS:ann%40x.io?secret=JBSWY3DPEHPK3PXP&issuer=OMS"), "JBSWY3DPEHPK3PXP");
  assert.equal(secretFromOtpauth("not a uri"), null);
});

test("return_to stays on this site", () => {
  assert.equal(safeReturnTo("/trade/"), "/trade/");
  assert.equal(safeReturnTo("/cockpit/users?x=1"), "/cockpit/users?x=1");
  assert.equal(safeReturnTo("//evil.example"), null);
  assert.equal(safeReturnTo("https://evil.example/"), null);
  assert.equal(safeReturnTo("/\\evil.example"), null);
  assert.equal(safeReturnTo(null), null);
  assert.equal(loginHref("/cockpit/", "/cockpit/users"), "/cockpit/login?return_to=%2Fcockpit%2Fusers");
});

test("the invite token comes from the URL fragment", () => {
  assert.equal(inviteTokenFromHash("#abc_DEF-123"), "abc_DEF-123");
  assert.equal(inviteTokenFromHash(""), null);
  assert.equal(inviteTokenFromHash("#"), null);
});
