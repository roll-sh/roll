import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { setActiveClient } from "@/lib/agents-client";
import { approval, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, M1 } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig()), ...(await import("@/test/api")).apiModule }));

import { Rolling } from "./rolling";

describe("Rolling", () => {
  afterEach(() => setActiveClient(null));

  it("lists busy threads and answers approvals from the header", async () => {
    const h = agentsHarness({
      entities: [
        mirrored("project", project(), 1),
        mirrored("thread", thread({ status: "waiting", activity: "Waiting: Run `cargo test`" }), 2),
        mirrored("thread", thread({ id: "thr_2", title: "Docs", status: "running", activity: "Editing README.md" }), 3),
        mirrored("request", approval(), 4),
      ],
    });
    setActiveClient(h.client);
    render(
      <MarkStateProvider>
        <Rolling />
      </MarkStateProvider>,
    );
    const button = await screen.findByRole("button", { name: /rolling/ });
    expect(button).toHaveTextContent("rolling2 · 1");
    await userEvent.click(button);
    const dialog = screen.getByRole("dialog", { name: "Rolling" });
    expect(dialog).toHaveTextContent("Editing README.md");
    await userEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(h.command).toHaveBeenCalledWith(M1, expect.stringMatching(/^cmd_/), { type: "request.respond", request: "req_1", response: { type: "decision", decision: "accept" } });
  });

  it("stays out of the way without agents", () => {
    const { container } = render(
      <MarkStateProvider>
        <Rolling />
      </MarkStateProvider>,
    );
    expect(container).toBeEmptyDOMElement();
  });
});
