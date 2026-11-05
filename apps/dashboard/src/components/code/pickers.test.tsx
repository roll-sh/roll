import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api, resetApi } from "@/test/api";
import { change } from "@/test/fixtures";
import { macClick } from "@/test/mac-click";
import { RequestReview } from "./review";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig<typeof import("@/lib/code-api")>()), ...(await import("@/test/api")).apiModule }));

/**
 * roll code's fields that name someone are pickers: a review is asked of roll, one of the owner's
 * agents or review apps picked by name, or a person typed as @name (there's no list of people).
 */

beforeEach(() => {
  resetApi();
  localStorage.clear();
});

const run = vi.fn(async (_key: string, fn: () => Promise<unknown>) => fn());

describe("asking for a review", () => {
  it("picks roll, an agent or a review app, or takes a typed @name", async () => {
    api.tokens.mockResolvedValue([
      { id: "tok_1", name: "me", created_at: 1, last_used_at: null, prefix: null },
      { id: "tok_2", name: "agent:triage", created_at: 1, last_used_at: null, prefix: null },
    ] as never);
    api.reviewApps.mockResolvedValue([{ id: "app_1", owner: "acme", name: "greptile" }] as never);
    api.requestReview.mockResolvedValue({} as never);
    render(<RequestReview change={change()} busy={null} run={run} />);
    await waitFor(() => expect(api.reviewApps).toHaveBeenCalledWith("acme"));
    macClick(screen.getByRole("button", { name: "Reviewer" }));
    const list = await screen.findByRole("listbox", { name: "Reviewer" });
    await waitFor(() => expect(within(list).getAllByRole("option").map((o) => o.textContent)).toEqual(["rollroll's own review", "triageagent", "greptilereview app"]));
    macClick(within(list).getByRole("option", { name: /greptile/ }));
    fireEvent.click(screen.getByTitle("Request a review"));
    await waitFor(() => expect(api.requestReview).toHaveBeenCalledWith(change().id, "app:greptile", false));

    macClick(screen.getByRole("button", { name: "Reviewer" }));
    fireEvent.change(await screen.findByRole("combobox", { name: "Search reviewer" }), { target: { value: "@ada" } });
    macClick(await screen.findByRole("option", { name: /Use @ada/ }));
    fireEvent.click(screen.getByTitle("Request a review"));
    await waitFor(() => expect(api.requestReview).toHaveBeenLastCalledWith(change().id, "@ada", false));
  });

  it("still offers roll and a typed name when the owner's lists aren't the caller's to see", async () => {
    api.tokens.mockRejectedValue(new Error("forbidden"));
    api.reviewApps.mockRejectedValue(new Error("forbidden"));
    render(<RequestReview change={change()} busy={null} run={run} />);
    macClick(screen.getByRole("button", { name: "Reviewer" }));
    const list = await screen.findByRole("listbox", { name: "Reviewer" });
    expect(within(list).getAllByRole("option")).toHaveLength(1);
    expect(within(list).getByRole("option", { name: /roll/ })).toBeInTheDocument();
  });
});
