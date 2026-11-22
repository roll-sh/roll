import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Catalog, Grant, Me, OAuthClient } from "@/lib/access-api";
import { Consent } from "./consent";

const CLIENT: OAuthClient = { id: "https://claude.ai/oauth/claude-code", name: "Claude Code", redirect_uris: ["http://127.0.0.1:33418/callback"], created_at: 1, last_used_at: null };
const ME: Me = { kind: "operator", user: { id: "usr_ada", handle: "ada" }, locked: true };
const CATALOG: Catalog = {
  products: ["postgres", "apps"],
  actions: ["read", "write", "delete", "reveal", "deploy", "admin"],
  presets: [{ id: "agent", name: "Agent", description: "Read and write", rules: [{ product: "*", resources: "*", actions: { read: "allow" } }] }],
  routes: [],
  operator_only: [],
  api_url: "http://127.0.0.1:7070",
};
const connection = {
  id: "tok_conn",
  name: "Claude Code",
  kind: "oauth",
  rules: [{ product: "postgres", resources: "*", actions: { read: "allow" } }],
  expires_at: null,
  ip_allow: [],
  created_by: "operator",
  created_at: 1,
  last_used_at: null,
  last_used_ip: null,
  revoked_at: null,
  client: CLIENT,
  user: "usr_ada",
} as Grant;
const authorize = vi.fn(async (_c: unknown) => ({ redirect: "http://127.0.0.1:33418/callback?code=x" }));
let grants: Grant[] = [connection];
vi.mock("@/lib/access-api", async (orig) => ({
  ...(await orig<typeof import("@/lib/access-api")>()),
  accessApi: { me: async () => ME, catalog: async () => CATALOG, client: async () => CLIENT, tokens: async () => grants, authorize: (c: unknown) => authorize(c) },
}));
vi.mock("@/lib/code-token", async (orig) => ({ ...(await orig<typeof import("@/lib/code-token")>()), requestSignIn: () => {}, subscribeToken: () => () => {} }));
vi.mock("../code/sign-in", () => ({ SignIn: () => null }));

const search = (scope: string) => ({
  client_id: CLIENT.id,
  redirect_uri: CLIENT.redirect_uris[0],
  code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
  response_type: "code",
  scope,
});

beforeEach(() => {
  authorize.mockClear();
  grants = [connection];
});

/** Step-up: a refused call's `roll:add:` scope is proposed, and allowing it widens the person's connection of that client. */
describe("consent: step-up", () => {
  it("proposes what the refused call needed and adds it to the existing connection", async () => {
    render(<Consent search={search("agent roll:add:postgres:delete:pa")} />);
    expect(await screen.findByText(/was refused and asks to be allowed/)).toBeTruthy();
    expect(screen.getByRole("checkbox", { name: /Add it to your connection/ })).toBeChecked();
    await userEvent.click(screen.getByRole("button", { name: /^Allow/ }));
    await waitFor(() => expect(authorize).toHaveBeenCalledTimes(1));
    const sent = authorize.mock.calls[0][0] as { widen?: string; rules: { product: string; resources: unknown; actions: Record<string, string> }[] };
    expect(sent.widen).toBe("tok_conn");
    expect(sent.rules).toContainEqual({ product: "postgres", resources: "*", actions: { read: "allow" } });
    expect(sent.rules).toContainEqual({ product: "postgres", resources: [{ id: "pa" }], actions: { delete: "allow" } });
  });

  it("makes a new connection when the person unticks it, and without step-up it's a plain consent", async () => {
    render(<Consent search={search("agent roll:add:postgres:delete:pa")} />);
    await userEvent.click(await screen.findByRole("checkbox", { name: /Add it to your connection/ }));
    await userEvent.click(screen.getByRole("button", { name: /^Allow/ }));
    await waitFor(() => expect(authorize).toHaveBeenCalledTimes(1));
    expect((authorize.mock.calls[0][0] as { widen?: string }).widen).toBeUndefined();
  });
});
