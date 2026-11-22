import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Catalog, Me, OAuthClient, StepUp } from "@/lib/access-api";
import { Consent } from "./consent";

const CLIENT: OAuthClient = { id: "https://claude.ai/oauth/claude-code", name: "Claude Code", redirect_uris: ["http://127.0.0.1:33418/callback"], created_at: 1, last_used_at: null };
const ME: Me = { kind: "operator", user: { id: "usr_ada", handle: "ada" }, locked: true };
const CATALOG: Catalog = {
  products: ["postgres", "apps"],
  actions: ["read", "write", "delete", "reveal", "deploy", "admin"],
  presets: [{ id: "agent", name: "Agent", description: "Read", rules: [{ product: "*", resources: "*", actions: { read: "allow" } }] }],
  routes: [],
  operator_only: [],
  api_url: "http://127.0.0.1:7070",
};
const proposal = (rule: StepUp["rule"]): StepUp => ({ id: "stp_abc", connection: { id: "tok_conn", name: "Claude Code on laptop", client_id: CLIENT.id }, rule, waits_for_admin: false });
let stepUp: () => Promise<StepUp> = async () => proposal({ product: "postgres", resources: [{ id: "pa" }], actions: { delete: "allow" } });
const authorize = vi.fn(async (_c: unknown) => ({ redirect: "http://127.0.0.1:33418/callback?code=x" }));
vi.mock("@/lib/access-api", async (orig) => ({
  ...(await orig<typeof import("@/lib/access-api")>()),
  accessApi: { me: async () => ME, catalog: async () => CATALOG, client: async () => CLIENT, stepUp: () => stepUp(), authorize: (c: unknown) => authorize(c) },
}));
vi.mock("@/lib/code-token", async (orig) => ({ ...(await orig<typeof import("@/lib/code-token")>()), requestSignIn: () => {}, subscribeToken: () => () => {} }));
vi.mock("../code/sign-in", () => ({ SignIn: () => null }));

const search = (scope: string) => ({ client_id: CLIENT.id, redirect_uri: CLIENT.redirect_uris[0], code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", response_type: "code", scope });

beforeEach(() => {
  authorize.mockClear();
  stepUp = async () => proposal({ product: "postgres", resources: [{ id: "pa" }], actions: { delete: "allow" } });
});

/** Step-up: a refused call's step-up is shown as roll recorded it, and adding it sends only its id. */
describe("consent: step-up", () => {
  it("shows the one rule and its connection, and adds it by id", async () => {
    render(<Consent search={search("roll:step-up:stp_abc")} />);
    expect(await screen.findByText(/asks for one more permission on .Claude Code on laptop./)).toBeTruthy();
    expect(screen.queryByText(/reaches far/)).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Add it" }));
    await waitFor(() => expect(authorize).toHaveBeenCalledTimes(1));
    expect(authorize.mock.calls[0][0]).toMatchObject({ step_up: "stp_abc", rules: [], client_id: CLIENT.id });
  });

  it("says when a rule reaches far, and a new connection is one click away", async () => {
    stepUp = async () => proposal({ product: "postgres", resources: "*", actions: { admin: "allow" } });
    render(<Consent search={search("roll:step-up:stp_abc")} />);
    expect(await screen.findByText(/reaches far/)).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: /new connection instead/ }));
    expect(await screen.findByRole("button", { name: /^Allow/ })).toBeTruthy();
  });

  it("is a plain consent when the step-up isn't the person's", async () => {
    stepUp = async () => {
      throw new Error("no such step-up of yours");
    };
    render(<Consent search={search("roll:step-up:stp_abc")} />);
    expect(await screen.findByRole("button", { name: /^Allow/ })).toBeTruthy();
    expect(screen.queryByText(/one more permission/)).toBeNull();
  });
});
