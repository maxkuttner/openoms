import { test } from "node:test";
import assert from "node:assert/strict";
import {
  accessMessage, authErrorMessage, isApprovalPending, isPendingAccount, loginMode, mfaPromptText, mfaStepFor, loginHref,
  nextStepFromLoginResponse, safeReturnTo, secretFromOtpauth, twoStepSection, visibleMethods,
} from "./login.ts";

test("only enabled sign-in methods are shown", () => {
  const off = { local: false, sso: false, sso_label: "Sign in with SSO" };
  assert.deepEqual(visibleMethods(off), []);
  assert.deepEqual(visibleMethods({ ...off, sso: true }), ["sso"]);
  assert.deepEqual(visibleMethods({ ...off, local: true }), ["local"]);
  assert.deepEqual(visibleMethods({ ...off, local: true, sso: true }), ["sso", "local"]);
});

test("the login response names the next step", () => {
  assert.equal(nextStepFromLoginResponse({ next: "totp" }), "totp");
  assert.equal(nextStepFromLoginResponse({ next: "totp_enroll" }), "totp_enroll");
  assert.equal(nextStepFromLoginResponse({ next: "done" }), "done");
  assert.equal(nextStepFromLoginResponse({ mfa_verified: true }), "done");
  assert.equal(nextStepFromLoginResponse(undefined), "done");
  assert.equal(nextStepFromLoginResponse({ next: "something_else" }), "done");
});

test("access reasons read as plain text", () => {
  assert.equal(accessMessage("no_account", null), "There is no account for this sign-in. Ask the OMS admin to add you.");
  assert.equal(accessMessage(null, null), "Access not granted. Ask the OMS admin to add you.");
});

test("a pending account is gated until approved", () => {
  assert.equal(isPendingAccount({ role: "pending" }), true);
  assert.equal(isPendingAccount({ role: "user" }), false);
  assert.equal(isPendingAccount(null), false);
  assert.equal(isApprovalPending(403, '{"error":"approval_pending"}'), true);
  assert.equal(isApprovalPending(403, '{"error":"mfa_required"}'), false);
  assert.equal(isApprovalPending(401, '{"error":"approval_pending"}'), false);
});

test("a fresh install offers to create the admin account", () => {
  const m = { local: true, sso: false, sso_label: "SSO" };
  assert.equal(loginMode({ ...m, first_run: true }), "create_admin");
  assert.equal(loginMode({ ...m, first_run: false }), "signin");
  assert.equal(loginMode({ ...m, local: false, first_run: true }), "signin", "no local sign-in, no form");
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

test("the MFA step follows how the session signed in", () => {
  assert.equal(mfaStepFor("local"), "totp");
  assert.equal(mfaStepFor("oidc"), "sso");
  assert.equal(mfaStepFor(null), "sso");
  assert.equal(mfaStepFor("local", "totp_not_enrolled"), "sso");
  assert.equal(mfaStepFor("local", "not_a_local_session"), "sso");
  assert.equal(mfaPromptText("totp"), "Confirm with your authenticator code to continue.");
  assert.equal(mfaPromptText("sso"), "Sign in again with SSO to continue.");
});

test("auth errors read by their code", () => {
  assert.equal(authErrorMessage(401, "unauthenticated", null), "Your sign-in expired. Sign in again.");
  assert.equal(authErrorMessage(401, "invalid_code", null), "That code didn't work. Try again.");
  assert.equal(authErrorMessage(401, "invalid_credentials", null), "Wrong email or password.");
  assert.equal(authErrorMessage(429, "too_many_attempts", null), "Too many attempts. Wait a few minutes and try again.");
  assert.equal(authErrorMessage(400, "weak_password", "too common"), "too common");
  assert.equal(authErrorMessage(500, null, null), "Something went wrong. Try again.");
});

test("two-step sign-in settings show only for local accounts", () => {
  assert.equal(twoStepSection({ auth_method: "local", totp_enrolled: false }), "enroll");
  assert.equal(twoStepSection({ auth_method: "local", totp_enrolled: true }), "disable");
  assert.equal(twoStepSection({ auth_method: "oidc", totp_enrolled: false }), "hidden");
  assert.equal(twoStepSection({ auth_method: "local" }), "enroll");
  assert.equal(twoStepSection(null), "hidden");
});
