import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { RequestCard } from "./request-card";
import { approval, question } from "@/test/agents";

describe("RequestCard", () => {
  it("approves, or from More options approves for the session or stops the run", async () => {
    for (const [label, decision] of [
      ["Approve", "accept"],
      ["Approve for session", "accept_for_session"],
      ["Stop run", "cancel"],
    ] as const) {
      const onRespond = vi.fn(async () => {});
      const { unmount } = render(<RequestCard request={approval()} onRespond={onRespond} />);
      expect(screen.getByText("cargo test -p code-control")).toBeInTheDocument();
      if (label === "Approve") await userEvent.click(screen.getByRole("button", { name: label }));
      else {
        await userEvent.click(screen.getByRole("button", { name: "More options" }));
        await userEvent.click(await screen.findByRole("menuitem", { name: label }));
      }
      expect(onRespond).toHaveBeenCalledWith({ type: "decision", decision });
      // Answered once: the buttons stay off until the request resolves.
      expect(screen.getByRole("button", { name: "More options" })).toBeDisabled();
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

  it("shows the error and lets you try again", async () => {
    const onRespond = vi.fn(async () => {
      throw new Error("the machine isn't connected");
    });
    render(<RequestCard request={approval()} onRespond={onRespond} />);
    await userEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("the machine isn't connected");
    expect(screen.getByRole("button", { name: "Approve" })).toBeEnabled();
  });

  it("answers questions one at a time: a choice moves on, Previous goes back, several or your own words", async () => {
    const onRespond = vi.fn(async () => {});
    render(<RequestCard request={question()} onRespond={onRespond} />);
    expect(screen.getByText("1/2")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Next question" })).toBeDisabled();
    // The digit picks (T3's 1–9), and one choice moves on by itself.
    fireEvent.keyDown(screen.getByRole("radio", { name: /SQLite/ }), { key: "2" });
    expect(await screen.findByText("What should run after the fix?")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Previous" }));
    expect(screen.getByRole("radio", { name: /Postgres/ })).toHaveAttribute("aria-checked", "true");
    await userEvent.click(screen.getByRole("button", { name: "Next question" }));
    const submit = screen.getByRole("button", { name: "Submit answers" });
    expect(submit).toBeDisabled();
    await userEvent.click(screen.getByRole("checkbox", { name: /Unit tests/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /Clippy/ }));
    await userEvent.type(screen.getByRole("textbox", { name: "Checks: your answer" }), "fmt");
    await userEvent.click(submit);
    expect(onRespond).toHaveBeenCalledWith({ type: "answers", answers: { db: "Postgres", checks: "Unit tests, Clippy, fmt" } });
  });
});
