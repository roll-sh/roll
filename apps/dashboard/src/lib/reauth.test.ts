import { afterEach, describe, expect, it, vi } from "vitest";
import { onReauthRequired } from "./reauth";
import { ApiError, call } from "./roll-api";

describe("sudo mode on a product's API", () => {
  afterEach(() => {
    onReauthRequired(null);
    vi.unstubAllGlobals();
  });
  it("a token made from a session confirms, then is made", async () => {
    const f = vi.fn();
    f.mockResolvedValueOnce(new Response(JSON.stringify({ error: "confirm it's you first", code: "reauth_required" }), { status: 403 }));
    f.mockResolvedValueOnce(new Response(JSON.stringify({ grant: { id: "grt_1" }, token: "rlt_x" }), { status: 201 }));
    vi.stubGlobal("fetch", f);
    const confirm = vi.fn(async () => {});
    onReauthRequired(confirm);
    await expect(call("POST", "/v1/access/tokens", { name: "ci" })).resolves.toMatchObject({ grant: { id: "grt_1" } });
    expect(f).toHaveBeenCalledTimes(2);
    expect(confirm).toHaveBeenCalledOnce();
  });
  it("any other refusal is the caller's, with its code", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(new Response(JSON.stringify({ error: "no", code: "forbidden" }), { status: 403 })));
    onReauthRequired(vi.fn(async () => {}));
    const e = await call("POST", "/v1/access/tokens", {}).catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect((e as ApiError).code).toBe("forbidden");
  });
});
