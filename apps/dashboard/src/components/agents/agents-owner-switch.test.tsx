import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

vi.mock("@/lib/code-api", () => ({
  codeApi: {
    info: async () => ({ auth: { required: true, caller: { kind: "admin", owner: null } } }),
    owners: async () => [{ name: "ada" }, { name: "bea" }],
  },
}));
vi.mock("@/lib/agents-client", () => ({ DEMO_OWNER: "demo", clientFor: (owner: string) => ({ owner }) }));
vi.mock("@/lib/agents-services", () => ({ httpServices: () => ({}) }));

const { chooseOwner, useAgentsConnection, useShownOwner } = await import("./use-connection");

describe("an admin's owner", () => {
  it("shows the first owner, then the one picked, in the agents and the shell alike", async () => {
    const { result } = renderHook(() => ({ conn: useAgentsConnection({}), shown: useShownOwner() }));
    await waitFor(() => expect(result.current.conn.client?.owner).toBe("ada"));
    expect(result.current.conn.admin).toBe(true);
    expect(result.current.shown).toEqual({ owner: "ada", admin: true });
    act(() => chooseOwner("bea"));
    await waitFor(() => expect(result.current.conn.client?.owner).toBe("bea"));
    await waitFor(() => expect(result.current.shown?.owner).toBe("bea"));
  });
});
