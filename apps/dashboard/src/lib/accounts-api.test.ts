import { describe, expect, it } from "vitest";
import { activityText, type Activity } from "./accounts-api";

describe("activityText", () => {
  const e = (a: Partial<Activity>): Activity => ({ at: 0, action: "sign_in", actor: "usr_me", ok: true, status: 200, ...a });
  it("says what happened to your account, and by whom", () => {
    expect(activityText(e({ detail: { method: "github" } }), "usr_me")).toBe("Signed in with GitHub");
    expect(activityText(e({ detail: { method: "email", second_factor: "owed" } }), "usr_me")).toBe("Signed in with an email code, then asked for your second factor");
    expect(activityText(e({ ok: false, detail: { method: "sso:sso_1", refused: "sso_required" } }), "usr_me")).toBe("A sign-in with your company's sign-in was refused (sso required)");
    expect(activityText(e({ action: "PATCH /v1/accounts/users/{id}", actor: "operator", detail: { set: { disabled: true } } }), "usr_me")).toBe("Your account was disabled by the operator");
    expect(activityText(e({ action: "PATCH /v1/accounts/users/{id}", actor: "usr_admin", detail: { set: { admin: true } } }), "usr_me")).toBe("An admin made you an admin");
    expect(activityText(e({ action: "scim.update", actor: "scim:sso_1", detail: { active: false } }), "usr_me")).toBe("Your company's provider deactivated your account");
    expect(activityText(e({ action: "DELETE /v1/accounts/me/totp" }), "usr_me")).toBe("Your authenticator app was turned off");
  });
});
