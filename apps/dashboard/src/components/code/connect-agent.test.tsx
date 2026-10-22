import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Activity, useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { OwnerTokenInfo } from "@/lib/code-api";
import { api, resetApi } from "@/test/api";
import { info, T0 } from "@/test/fixtures";
import { AccessTokens, ConnectAgent } from "./connect-agent";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig<typeof import("@/lib/code-api")>()), ...(await import("@/test/api")).apiModule }));

/**
 * The owner page's token panel: a token made on the spot is shown once (in the MCP snippet), and
 * the list only ever shows prefixes. Wired like CodeOwner: creating one reloads the list.
 */

const SECRET = "rlc_ab12cd34_THE_SECRET_PART_9f8e7d6c5b4a";
const existing: OwnerTokenInfo = { id: "tok_old", name: "ci", created_at: T0, last_used_at: null, prefix: "rlc_zz99" };
const created: OwnerTokenInfo = { id: "tok_new", name: "ci-bot", created_at: T0 + 10, last_used_at: null, prefix: "rlc_ab12" };
const signedIn = info({ required: true, caller: { kind: "owner", owner: "acme" } });

function OwnerTokens() {
  const [tick, setTick] = useState(0);
  return (
    <>
      <ConnectAgent owner="acme" info={signedIn} manage onTokensChanged={() => setTick((n) => n + 1)} />
      <AccessTokens owner="acme" reload={tick} />
    </>
  );
}

const snippet = () => document.querySelector("pre")!;
const tokenList = () => screen.getByRole("heading", { name: "Access tokens" }).closest("section")!;
/** How many times the secret appears anywhere in the page's text or attributes. */
const secretCount = () => (document.body.outerHTML.match(new RegExp(SECRET, "g")) ?? []).length;

beforeEach(() => {
  resetApi();
  api.tokens.mockResolvedValue([existing]);
  api.createToken.mockResolvedValue({ token: SECRET, id: "tok_new", prefix: "rlc_ab12", name: "ci-bot", owner: "acme", created_at: T0 + 10 });
});

async function createOne() {
  const u = userEvent.setup();
  await screen.findByText("ci");
  await u.clear(screen.getByRole("textbox", { name: "Token name" }));
  await u.type(screen.getByRole("textbox", { name: "Token name" }), "ci-bot");
  api.tokens.mockResolvedValue([existing, created]);
  await u.click(screen.getByRole("button", { name: /Create a token for acme/ }));
  await screen.findByText(/shown once: copy it now/);
  return u;
}

describe("ConnectAgent and AccessTokens: a new token is shown once", () => {
  it("before creating one, the snippet has a placeholder and the list shows prefixes", async () => {
    render(<OwnerTokens />);
    const row = (await screen.findByText("ci")).closest("li")!;
    expect(row).toHaveTextContent("rlc_zz99…");
    expect(row).toHaveTextContent("never used");
    expect(snippet()).toHaveTextContent('--header "Authorization: Bearer $ROLL_CODE_TOKEN"');
    expect(snippet()).toHaveTextContent("claude mcp add --transport http roll http://roll.test/v1/code/mcp");
  });

  it("shows the new token in the snippet, once, and the reloaded list shows only its prefix", async () => {
    render(<OwnerTokens />);
    await createOne();
    expect(api.createToken).toHaveBeenCalledWith("acme", "ci-bot");
    expect(snippet()).toHaveTextContent(`Authorization: Bearer ${SECRET}`);
    expect(screen.getByText(/shown once: copy it now/)).toHaveTextContent("It acts as acme");
    // The create form is gone: one token per visit.
    expect(screen.queryByRole("button", { name: /Create a token/ })).toBeNull();

    // The list was fetched again and has the new token, by prefix.
    await waitFor(() => expect(within(tokenList()).getByText("ci-bot")).toBeInTheDocument());
    expect(api.tokens).toHaveBeenCalledTimes(2);
    expect(within(tokenList()).getByText("ci-bot").closest("li")).toHaveTextContent("rlc_ab12…");
    expect(within(tokenList()).getByText("2")).toBeInTheDocument();
    expect(tokenList().innerHTML).not.toContain(SECRET);
    expect(tokenList().innerHTML).not.toContain("THE_SECRET_PART");
    expect(secretCount()).toBe(1);
  });

  it("the JSON format carries it too, still only once on the page", async () => {
    render(<OwnerTokens />);
    const u = await createOne();
    await u.click(screen.getByRole("radio", { name: "JSON" }));
    expect(JSON.parse(snippet().textContent!).mcpServers.roll.headers.Authorization).toBe(`Bearer ${SECRET}`);
    expect(secretCount()).toBe(1);
  });

  it("is gone after leaving the page and coming back", async () => {
    const { unmount } = render(<OwnerTokens />);
    await createOne();
    unmount();
    render(<OwnerTokens />);
    await screen.findByText("ci");
    expect(secretCount()).toBe(0);
    expect(snippet()).toHaveTextContent("$ROLL_CODE_TOKEN");
    expect(screen.getByRole("button", { name: /Create a token for acme/ })).toBeInTheDocument();
  });

  it("is gone after Next hides the page and shows it again (cacheComponents keeps pages in <Activity>)", async () => {
    const page = (mode: "visible" | "hidden") => (
      <Activity mode={mode}>
        <OwnerTokens />
      </Activity>
    );
    const { rerender } = render(page("visible"));
    await createOne();
    rerender(page("hidden"));
    rerender(page("visible"));
    await waitFor(() => expect(secretCount()).toBe(0));
    expect(snippet()).toHaveTextContent("$ROLL_CODE_TOKEN");
    expect(screen.getByRole("button", { name: /Create a token for acme/ })).toBeInTheDocument();
  });

  it("stays shown once while the page re-renders, and never enters the list", async () => {
    const { rerender } = render(<OwnerTokens />);
    await createOne();
    rerender(<OwnerTokens />);
    expect(secretCount()).toBe(1);
    expect(tokenList().innerHTML).not.toContain(SECRET);
  });

  it("re-fetching the list never brings a secret into it, whatever the API sends", async () => {
    api.tokens.mockResolvedValue([{ ...created, token: SECRET } as OwnerTokenInfo]);
    const { rerender } = render(<AccessTokens owner="acme" reload={0} />);
    await screen.findByText("ci-bot");
    api.tokens.mockResolvedValue([{ ...created, token: SECRET } as OwnerTokenInfo, { ...existing, prefix: null }]);
    rerender(<AccessTokens owner="acme" reload={1} />);
    // A token from before prefixes shows its id.
    expect(await screen.findByText("tok_old")).toBeInTheDocument();
    expect(api.tokens).toHaveBeenCalledTimes(2);
    expect(secretCount()).toBe(0);
  });

  it("a failed create says why and shows no token", async () => {
    api.createToken.mockRejectedValue(new Error("token limit reached"));
    render(<OwnerTokens />);
    const u = userEvent.setup();
    await screen.findByText("ci");
    await u.click(screen.getByRole("button", { name: /Create a token for acme/ }));
    expect(await screen.findByText("token limit reached")).toBeInTheDocument();
    expect(snippet()).toHaveTextContent("$ROLL_CODE_TOKEN");
    expect(api.tokens).toHaveBeenCalledTimes(1);
  });

  it("revoking one removes it from the list", async () => {
    api.revokeToken.mockResolvedValue(undefined);
    render(<OwnerTokens />);
    await screen.findByText("ci");
    api.tokens.mockResolvedValue([]);
    await userEvent.click(screen.getByRole("button", { name: "Revoke ci" }));
    expect(api.revokeToken).toHaveBeenCalledWith("acme", "tok_old");
    expect(await screen.findByText(/No tokens yet/)).toBeInTheDocument();
  });

  it("an open daemon needs no token: no header, no create form", async () => {
    render(<ConnectAgent owner="acme" info={info()} manage />);
    expect(snippet()).not.toHaveTextContent("Authorization");
    expect(screen.getByText(/This daemon is open, so no token is needed/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Create a token/ })).toBeNull();
  });

  it("people who can't manage the owner get the CLI hint instead of a form", async () => {
    render(<ConnectAgent owner="acme" info={info({ required: true, caller: { kind: "anonymous", owner: null } })} manage={false} />);
    expect(screen.getByText("roll code token create acme agent")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Create a token/ })).toBeNull();
  });
});
