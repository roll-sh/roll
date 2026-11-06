import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { InboxToast, showInboxToast } from "./threads-sidebar";

const notice = () => document.querySelector<HTMLElement>("[data-place]");

describe("the undo notice's motion", () => {
  it("sinks out after Undo, no longer a status and inert meanwhile, then it's gone", async () => {
    render(<InboxToast />);
    let undone = 0;
    act(() => showInboxToast({ text: "Settled “Bump tokio”", undo: () => void undone++ }));
    expect(screen.getByRole("status")).toHaveTextContent("Settled “Bump tokio”");
    await userEvent.click(screen.getByRole("button", { name: "Undo" }));
    expect(undone).toBe(1);
    // Still drawn as it goes: not announced, and a second click can't undo twice.
    expect(screen.queryByRole("status")).toBeNull();
    expect(notice()).toHaveAttribute("inert");
    await waitFor(() => expect(notice()).toBeNull());
  });

  it("a new notice replaces the old one: the old one leaves, the new one is the status", async () => {
    render(<InboxToast />);
    act(() => showInboxToast({ text: "Settled “Bump tokio”" }));
    act(() => showInboxToast({ text: "Snoozed “Fix the flaky test”" }));
    expect(screen.getByRole("status")).toHaveTextContent("Snoozed “Fix the flaky test”");
    await waitFor(() => expect(document.querySelectorAll("[data-place]")).toHaveLength(1));
    act(() => showInboxToast(null));
  });
});
