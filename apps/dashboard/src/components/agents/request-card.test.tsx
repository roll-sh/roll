import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { RequestCard } from "./request-card";
import { approval, question } from "@/test/agents";

describe("RequestCard", () => {
  it("approves, approves for the session, or stops the run", async () => {
    for (const [label, decision] of [
      ["Approve", "accept"],
      ["Approve for session", "accept_for_session"],
      ["Stop run", "cancel"],
    ] as const) {
      const onRespond = vi.fn(async () => {});
      const { unmount } = render(<RequestCard request={approval()} onRespond={onRespond} />);
      expect(screen.getByText("Run `cargo test -p code-control`")).toBeInTheDocument();
      expect(screen.getByText("cargo test -p code-control")).toBeInTheDocument();
      await userEvent.click(screen.getByRole("button", { name: label }));
      expect(onRespond).toHaveBeenCalledWith({ type: "decision", decision });
      // Answered once: the buttons stay off until the request resolves.
      expect(screen.getByRole("button", { name: "Approve for session" })).toBeDisabled();
      unmount();
    }
  });

  it("declines with a note back to the agent", async () => {
    const onRespond = vi.fn(async () => {});
    render(<RequestCard request={approval()} onRespond={onRespond} />);
    await userEvent.click(screen.getByRole("button", { name: "Decline" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Note to the agent" }), "use the fake runner{Enter}");
    expect(onRespond).toHaveBeenCalledWith({ type: "decision", decision: "decline", message: "use the fake runner" });
  });

  it("only offers the decisions the provider accepts", () => {
    const q = approval();
    if (q.body.type === "approval") q.body.decisions = ["accept", "decline"];
    render(<RequestCard request={q} onRespond={async () => {}} />);
    expect(screen.queryByRole("button", { name: "Approve for session" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Stop run" })).not.toBeInTheDocument();
  });

  it("shows the error and lets you try again", async () => {
    const onRespond = vi.fn(async () => {
      throw new Error("the machine isn't connected");
    });
    render(<RequestCard request={approval()} onRespond={onRespond} />);
    await userEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("the machine isn't connected");
    expect(screen.getByRole("button", { name: "Approve" })).toBeEnabled();
  });

  it("answers questions: one option, several, or your own words", async () => {
    const onRespond = vi.fn(async () => {});
    render(<RequestCard request={question()} onRespond={onRespond} />);
    const answer = screen.getByRole("button", { name: "Answer" });
    expect(answer).toBeDisabled();
    await userEvent.click(screen.getByRole("radio", { name: /Postgres/ }));
    expect(answer).toBeDisabled();
    await userEvent.click(screen.getByRole("checkbox", { name: /Unit tests/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /Clippy/ }));
    const checks = screen.getByRole("group", { name: "Checks" });
    await userEvent.click(checks.querySelector("button:last-of-type")!);
    await userEvent.type(screen.getByRole("textbox", { name: "Checks: your answer" }), "fmt");
    await userEvent.click(answer);
    expect(onRespond).toHaveBeenCalledWith({ type: "answers", answers: { db: "Postgres", checks: "Unit tests, Clippy, fmt" } });
  });

  it("records what was decided once resolved", () => {
    render(
      <RequestCard
        request={approval({ status: "resolved", resolved_by: "ada", response: { type: "decision", decision: "decline", message: "not now" } })}
        onRespond={async () => {}}
      />,
    );
    expect(screen.getByText("Declined: “not now”")).toBeInTheDocument();
    expect(screen.getByText("by ada")).toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });
});
