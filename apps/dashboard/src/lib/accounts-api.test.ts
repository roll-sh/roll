import { afterEach, describe, expect, it, vi } from "vitest";
import { accountsApi, activityText, dormant, onReauthRequired, type Activity } from "./accounts-api";

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

describe("an export in the security log", () => {
  it("says so", () => {
    expect(activityText({ at: 0, action: "account.export", actor: "usr_me", ok: true, status: 200 }, "usr_me")).toBe("You exported your data");
  });
});

describe("second-factor resets in the security log", () => {
  const e = (a: Partial<Activity>): Activity => ({ at: 0, action: "sign_in", actor: "usr_me", ok: true, status: 200, ...a });
  it("say who asked and who decided", () => {
    expect(activityText(e({ action: "recovery.request" }), "usr_me")).toBe("You asked an admin to reset your second factor");
    expect(activityText(e({ action: "recovery.approve", actor: "usr_admin" }), "usr_me")).toBe("Your second factor was reset by an admin");
    expect(activityText(e({ action: "recovery.deny", actor: "operator" }), "usr_me")).toBe("The operator declined to reset your second factor");
  });
});

describe("refused changes in the security log", () => {
  const e = (a: Partial<Activity>): Activity => ({ at: 0, action: "sign_in", actor: "usr_me", ok: true, status: 200, ...a });
  it("say what was refused, never what it would have done", () => {
    expect(activityText(e({ action: "DELETE /v1/accounts/me/passkeys/{id}" }), "usr_me")).toBe("A passkey was removed");
    expect(activityText(e({ action: "DELETE /v1/accounts/me/passkeys/{id}", ok: false, status: 403 }), "usr_me")).toBe("Removing a passkey was refused");
    expect(activityText(e({ action: "POST /v1/accounts/me/totp", ok: false, status: 403 }), "usr_me")).toBe("Setting up an authenticator app was refused");
    expect(activityText(e({ action: "PATCH /v1/accounts/users/{id}", actor: "usr_admin", ok: false, status: 403, detail: { set: { admin: true } } }), "usr_me")).toBe("Making you an admin was refused");
  });
});

describe("sudo mode", () => {
  afterEach(() => {
    onReauthRequired(null);
    vi.unstubAllGlobals();
  });
  const answers = (...bodies: [number, unknown][]) => {
    const f = vi.fn();
    for (const [status, body] of bodies) f.mockResolvedValueOnce(new Response(JSON.stringify(body), { status }));
    vi.stubGlobal("fetch", f);
    return f;
  };
  it("asks the person to confirm, then runs the change once more", async () => {
    const f = answers([403, { error: "confirm it's you first", code: "reauth_required" }], [200, { id: "pk_1", name: "Phone" }]);
    const confirm = vi.fn(async () => {});
    onReauthRequired(confirm);
    await expect(accountsApi.renamePasskey("pk_1", "Phone")).resolves.toMatchObject({ name: "Phone" });
    expect(confirm).toHaveBeenCalledOnce();
    expect(f).toHaveBeenCalledTimes(2);
  });
  it("changes nothing when the person doesn't confirm, or nobody can ask", async () => {
    const f = answers([403, { error: "confirm it's you first", code: "reauth_required" }]);
    onReauthRequired(async () => {
      throw new Error("Not confirmed: nothing changed.");
    });
    await expect(accountsApi.removePasskey("pk_1")).rejects.toThrow("Not confirmed");
    expect(f).toHaveBeenCalledOnce();
    onReauthRequired(null);
    answers([403, { error: "confirm it's you first", code: "reauth_required" }]);
    await expect(accountsApi.removePasskey("pk_1")).rejects.toMatchObject({ code: "reauth_required" });
  });
});

describe("a provider's logout in the security log", () => {
  it("says the provider signed the person out", () => {
    expect(activityText({ at: 0, action: "session.end", actor: "sso:sso_okta", ok: true, status: 200 }, "usr_me")).toBe("Your company's provider signed you out (its back-channel logout)");
  });
});

describe("dormant", () => {
  it("is 90 days without using roll, counted from when they were made if never seen", () => {
    const day = 24 * 3600 * 1000, now = 1000 * day;
    expect(dormant({ last_active_at: now - 89 * day, created_at: 0, disabled: false }, now)).toBe(false);
    expect(dormant({ last_active_at: now - 91 * day, created_at: 0, disabled: false }, now)).toBe(true);
    expect(dormant({ last_active_at: null, created_at: now - 91 * day, disabled: false }, now)).toBe(true);
    expect(dormant({ last_active_at: null, created_at: now - 91 * day, disabled: true }, now)).toBe(false);
  });
});
