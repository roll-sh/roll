import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Change } from "@/lib/code-api";
import { pendingKey } from "@/lib/review";
import { api, resetApi } from "@/test/api";
import { change, fileDiff, info, repo, review, T0, thread } from "@/test/fixtures";
import { MarkStateProvider } from "../mark-state";
import { CodeChange } from "./changes";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig<typeof import("@/lib/code-api")>()), ...(await import("@/test/api")).apiModule }));
// The page frame (sidebar, ⌘K palette, sign-in) isn't under test.
vi.mock("./shell", () => ({
  CodeShell: ({ actions, children }: { crumb: ReactNode; actions?: ReactNode; children: ReactNode }) => (
    <div>
      <header>{actions}</header>
      {children}
    </div>
  ),
}));

/**
 * The change page's review flow end to end against a fake API: click a line, write, post it now
 * or batch it into a pending review, submit with a verdict.
 */

beforeEach(() => {
  resetApi();
  api.info.mockResolvedValue(info());
  api.repo.mockResolvedValue(repo());
  api.change.mockResolvedValue(change());
  api.compare.mockResolvedValue({ base: "cs_base000000", head: "cs_head000000", changesets: [], files: [fileDiff()] });
  api.reviews.mockResolvedValue([]);
  api.threads.mockResolvedValue([]);
  api.reviewStatus.mockRejectedValue(new Error("no status"));
  api.reviewRequests.mockResolvedValue([]);
  api.aiJobs.mockResolvedValue([]);
  api.statuses.mockResolvedValue({ changeset: "cs_head000000", state: "success", statuses: [] } as never);
});

async function openChange() {
  const u = userEvent.setup();
  const view = render(
    <MarkStateProvider>
      <CodeChange fullName="acme/web" id="7" />
    </MarkStateProvider>,
  );
  await screen.findByRole("button", { name: "Comment on line 3" });
  return { u, ...view };
}

const composerBox = () => screen.getByPlaceholderText(/Leave a comment/);
const reviewComposer = () => document.getElementById("review-composer")!;

describe("CodeChange: commenting on a line", () => {
  it("posts a single-line comment on the new side right away", async () => {
    api.createThread.mockResolvedValue(thread({ id: "thr_new", line: 3, start_line: 3 }));
    const { u } = await openChange();
    expect(screen.getByText("Click a line number to comment; shift-click for a range.")).toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: "Comment on line 3" }));
    expect(screen.getByText(/^Comment on/, { selector: "p" })).toHaveTextContent("Comment on src/app.ts · L3");
    await u.type(composerBox(), "Why four?");
    await u.click(screen.getByRole("button", { name: "Comment now" }));
    await waitFor(() => expect(api.createThread).toHaveBeenCalledTimes(1));
    expect(api.createThread).toHaveBeenCalledWith("chg_1", { path: "src/app.ts", side: "new", line: 3, body: "Why four?" });
    await waitFor(() => expect(screen.queryByPlaceholderText(/Leave a comment/)).toBeNull());
  });

  it("shift-click makes a range: line is the last, start_line the first", async () => {
    api.createThread.mockResolvedValue(thread());
    const { u } = await openChange();
    await u.click(screen.getByRole("button", { name: "Comment on line 2" }));
    await u.keyboard("{Shift>}");
    await u.click(screen.getByRole("button", { name: "Comment on line 4" }));
    await u.keyboard("{/Shift}");
    expect(screen.getByText("L2–L4")).toBeInTheDocument();
    await u.type(composerBox(), "This block");
    await u.click(screen.getByRole("button", { name: "Comment now" }));
    await waitFor(() =>
      expect(api.createThread).toHaveBeenCalledWith("chg_1", { path: "src/app.ts", side: "new", line: 4, start_line: 2, body: "This block" }),
    );
  });

  it("comments on a removed line go on the old side, without Suggest", async () => {
    api.createThread.mockResolvedValue(thread({ side: "old" }));
    const { u } = await openChange();
    await u.click(screen.getByRole("button", { name: "Comment on old line 2" }));
    expect(screen.getByText(/L2\s*\(removed\)/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Suggest" })).toBeNull();
    await u.type(composerBox(), "Keep this");
    await u.click(screen.getByRole("button", { name: "Comment now" }));
    await waitFor(() => expect(api.createThread).toHaveBeenCalledWith("chg_1", { path: "src/app.ts", side: "old", line: 2, body: "Keep this" }));
  });

  it("Suggest drops the selected lines into a suggestion block", async () => {
    api.createThread.mockResolvedValue(thread());
    const { u } = await openChange();
    await u.click(screen.getByRole("button", { name: "Comment on line 2" }));
    await u.keyboard("{Shift>}");
    await u.click(screen.getByRole("button", { name: "Comment on line 3" }));
    await u.keyboard("{/Shift}");
    await u.type(composerBox(), "Rename:");
    await u.click(screen.getByRole("button", { name: "Suggest" }));
    expect(composerBox()).toHaveValue("Rename:\n\n```suggestion\nconst b = 3;\nconst c = 4;\n```");
    await u.click(screen.getByRole("button", { name: "Comment now" }));
    await waitFor(() =>
      expect(api.createThread).toHaveBeenCalledWith("chg_1", {
        path: "src/app.ts",
        side: "new",
        line: 3,
        start_line: 2,
        body: "Rename:\n\n```suggestion\nconst b = 3;\nconst c = 4;\n```",
      }),
    );
  });

  it("a failed post keeps the composer and what was written, and says why", async () => {
    api.createThread.mockRejectedValue(new Error("line 3 isn't in the diff"));
    const { u } = await openChange();
    await u.click(screen.getByRole("button", { name: "Comment on line 3" }));
    await u.type(composerBox(), "Hmm");
    await u.click(screen.getByRole("button", { name: "Comment now" }));
    expect(await screen.findByText("line 3 isn't in the diff")).toBeInTheDocument();
    expect(composerBox()).toHaveValue("Hmm");
  });

  it("Cancel and Escape close the composer without a request", async () => {
    const { u } = await openChange();
    await u.click(screen.getByRole("button", { name: "Comment on line 3" }));
    await u.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByPlaceholderText(/Leave a comment/)).toBeNull();
    await u.click(screen.getByRole("button", { name: "Comment on line 3" }));
    await u.type(composerBox(), "x{Escape}");
    expect(screen.queryByPlaceholderText(/Leave a comment/)).toBeNull();
    expect(api.createThread).not.toHaveBeenCalled();
  });

  it("existing threads render at their lines on the page", async () => {
    api.threads.mockResolvedValue([thread({ id: "t9", line: 3, start_line: 3, body: "Existing remark" })]);
    await openChange();
    const card = await waitFor(() => {
      const el = document.getElementById("thread-t9");
      if (!el) throw new Error("no thread yet");
      return el;
    });
    expect(card).toHaveTextContent("Existing remark");
    expect(card.closest("tr")!.previousElementSibling).toHaveTextContent("const c = 4;");
    // Signed in: it can be replied to and resolved.
    expect(within(card).getByRole("button", { name: /Reply/ })).toBeInTheDocument();
    expect(within(card).getByRole("button", { name: /Resolve/ })).toBeInTheDocument();
  });
});

describe("CodeChange: a pending review", () => {
  it("batches comments into a pending review and submits them with the verdict", async () => {
    api.submitReview.mockResolvedValue(review({ verdict: "approve" }));
    const { u } = await openChange();

    await u.click(screen.getByRole("button", { name: "Comment on line 3" }));
    await u.type(composerBox(), "First");
    await u.click(screen.getByRole("button", { name: "Start a review" }));
    expect(screen.queryByPlaceholderText(/Leave a comment/)).toBeNull();

    // The draft sits at its line, marked pending; nothing was sent.
    const draft = screen.getByText("First", { selector: ".markdown p" }).closest(".border-dashed")!;
    expect(within(draft as HTMLElement).getByText("pending")).toBeInTheDocument();
    expect(draft.closest("tr")!.previousElementSibling).toHaveTextContent("const c = 4;");
    expect(api.createThread).not.toHaveBeenCalled();
    expect(api.submitReview).not.toHaveBeenCalled();

    // The second one is added to it; a range on the old side.
    await u.click(screen.getByRole("button", { name: "Comment on old line 1" }));
    await u.keyboard("{Shift>}");
    await u.click(screen.getByRole("button", { name: "Comment on old line 2" }));
    await u.keyboard("{/Shift}");
    await u.type(composerBox(), "Second");
    await u.click(screen.getByRole("button", { name: "Add to review" }));

    const rc = reviewComposer();
    expect(within(rc).getByText("2 pending comments")).toBeInTheDocument();
    expect(within(rc).getByText("src/app.ts:L3")).toBeInTheDocument();
    expect(within(rc).getByText("src/app.ts:L1–L2")).toBeInTheDocument();
    // The header's Review button counts them.
    expect(screen.getByRole("button", { name: /Review\s*2/ })).toBeInTheDocument();
    // Kept in local storage until submitted.
    expect(JSON.parse(localStorage.getItem(pendingKey("chg_1"))!).comments).toHaveLength(2);

    await u.click(within(rc).getByRole("radio", { name: "Approve" }));
    await u.type(within(rc).getByPlaceholderText(/why it's good to land/), "Ship it");
    await u.click(within(rc).getByRole("button", { name: "Approve rev 2" }));

    await waitFor(() => expect(api.submitReview).toHaveBeenCalledTimes(1));
    expect(api.submitReview).toHaveBeenCalledWith("chg_1", {
      verdict: "approve",
      body: "Ship it",
      comments: [
        { path: "src/app.ts", side: "new", line: 3, body: "First" },
        { path: "src/app.ts", side: "old", line: 2, start_line: 1, body: "Second" },
      ],
    });
    // Submitted: the drafts and the stored review are gone, and the verdict resets.
    await waitFor(() => expect(within(rc).queryByText(/pending comment/)).toBeNull());
    expect(screen.queryByText("First")).toBeNull();
    expect(localStorage.getItem(pendingKey("chg_1"))).toBeNull();
    expect(within(rc).getByRole("radio", { name: "Comment" })).toHaveAttribute("aria-checked", "true");
  });

  it("request changes needs words or comments; comment-only reviews too", async () => {
    api.submitReview.mockResolvedValue(review({ verdict: "request_changes" }));
    const { u } = await openChange();
    const rc = reviewComposer();
    expect(within(rc).getByRole("button", { name: "Submit review" })).toBeDisabled();
    await u.click(within(rc).getByRole("radio", { name: "Request changes" }));
    const submit = within(rc).getByRole("button", { name: "Request changes" });
    expect(submit).toBeDisabled();
    await u.type(within(rc).getByPlaceholderText(/Summary of your review/), "Needs tests");
    expect(submit).toBeEnabled();
    await u.click(submit);
    await waitFor(() => expect(api.submitReview).toHaveBeenCalledWith("chg_1", { verdict: "request_changes", body: "Needs tests" }));
  });

  it("edits and removes drafts before submitting", async () => {
    api.submitReview.mockResolvedValue(review());
    const { u } = await openChange();
    for (const [line, body] of [
      ["Comment on line 1", "one"],
      ["Comment on line 4", "two"],
    ] as const) {
      await u.click(screen.getByRole("button", { name: line }));
      await u.type(composerBox(), body);
      await u.click(screen.getByRole("button", { name: /Start a review|Add to review/ }));
    }
    const [first, second] = screen.getAllByRole("button", { name: "Remove pending comment" });
    await u.click(second);
    expect(screen.queryByText("two")).toBeNull();
    await u.click(screen.getAllByRole("button", { name: "Edit pending comment" })[0]);
    const edit = screen.getByDisplayValue("one");
    await u.clear(edit);
    await u.type(edit, "one, edited");
    await u.click(screen.getByRole("button", { name: "Save" }));
    expect(first).toBeInTheDocument();
    await u.click(within(reviewComposer()).getByRole("button", { name: "Submit review" }));
    await waitFor(() =>
      expect(api.submitReview).toHaveBeenCalledWith("chg_1", {
        verdict: "comment",
        comments: [{ path: "src/app.ts", side: "new", line: 1, body: "one, edited" }],
      }),
    );
  });

  it("survives a reload, and discarding clears it", async () => {
    const { u, unmount } = await openChange();
    await u.click(screen.getByRole("button", { name: "Comment on line 3" }));
    await u.type(composerBox(), "Remember me");
    await u.click(screen.getByRole("button", { name: "Start a review" }));
    unmount();

    const again = await openChange();
    expect(await screen.findByText("Remember me", { selector: ".markdown p" })).toBeInTheDocument();
    expect(within(reviewComposer()).getByText("1 pending comment")).toBeInTheDocument();
    await again.u.click(within(reviewComposer()).getByRole("button", { name: "Discard pending review" }));
    expect(screen.queryByText("Remember me")).toBeNull();
    expect(localStorage.getItem(pendingKey("chg_1"))).toBeNull();
  });

  it("warns when the pending comments were written on an older revision", async () => {
    localStorage.setItem(
      pendingKey("chg_1"),
      JSON.stringify({ change: "chg_1", revision: 1, body: "", comments: [{ id: "d1", path: "src/app.ts", line: 3, side: "new", body: "old draft" }] }),
    );
    await openChange();
    expect(await within(reviewComposer()).findByText(/written on rev 1; lines may have moved/)).toBeInTheDocument();
  });

  it("a failed submit keeps the pending review", async () => {
    api.submitReview.mockRejectedValue(new Error("comment 1: line 3 is outside the diff"));
    const { u } = await openChange();
    await u.click(screen.getByRole("button", { name: "Comment on line 3" }));
    await u.type(composerBox(), "Keep");
    await u.click(screen.getByRole("button", { name: "Start a review" }));
    await u.click(within(reviewComposer()).getByRole("button", { name: "Submit review" }));
    expect(await screen.findByText("comment 1: line 3 is outside the diff")).toBeInTheDocument();
    expect(within(reviewComposer()).getByText("1 pending comment")).toBeInTheDocument();
    expect(localStorage.getItem(pendingKey("chg_1"))).not.toBeNull();
  });

  it("closed changes can't be commented on", async () => {
    api.change.mockResolvedValue(change({ state: "landed", landed_as: "cs_landed0000" }));
    render(
      <MarkStateProvider>
        <CodeChange fullName="acme/web" id="7" />
      </MarkStateProvider>,
    );
    await screen.findByText("const c = 4;");
    await act(async () => {});
    expect(screen.queryByRole("button", { name: /Comment on/ })).toBeNull();
    expect(document.getElementById("review-composer")).toBeNull();
  });
});

describe("CodeChange: landing and the merge queue", () => {
  const queued = (over: Partial<NonNullable<Change["queue"]>> = {}): NonNullable<Change["queue"]> => ({
    id: "qe_1",
    position: 3,
    lane: "normal",
    state: "waiting",
    error: null,
    output: null,
    batch: null,
    enqueued_at: T0,
    enqueued_by: null,
    finished_at: null,
    ...over,
  });

  it("queue policy: Add to queue and Urgent enqueue in their lanes", async () => {
    api.enqueue.mockResolvedValue({ status: "queued", position: 3, queue_id: "qe_1", change: change({ state: "queued" }) });
    const { u } = await openChange();
    await u.click(screen.getByRole("button", { name: "Urgent" }));
    await waitFor(() => expect(api.enqueue).toHaveBeenLastCalledWith("chg_1", "urgent"));
    api.change.mockResolvedValue(change({ state: "queued", queue: queued() }));
    await u.click(screen.getByRole("button", { name: "Add to queue" }));
    await waitFor(() => expect(api.enqueue).toHaveBeenLastCalledWith("chg_1", undefined));
    expect(await screen.findByText(/Queued at position 3\. It lands once its batch passes the queue's checks\./)).toBeInTheDocument();
    expect(screen.getByText("queued", { selector: "span" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Remove from queue" })).toBeInTheDocument();
    expect(api.landChange).not.toHaveBeenCalled();
  });

  it("post_merge policy: Land lands, with no urgent lane", async () => {
    api.repo.mockResolvedValue(repo({ land_policy: "post_merge" }));
    api.landChange.mockResolvedValue({ status: "landed", changeset: "cs_landed0000", git_commit: null, ref: "main", generation: 4 });
    const { u } = await openChange();
    expect(screen.queryByRole("button", { name: "Urgent" })).toBeNull();
    await u.click(screen.getByRole("button", { name: "Land" }));
    await waitFor(() => expect(api.landChange).toHaveBeenCalledWith("chg_1"));
    expect(api.enqueue).not.toHaveBeenCalled();
  });

  it("blockers keep the queue buttons disabled", async () => {
    api.change.mockResolvedValue(change({ blockers: [{ kind: "checks", message: "test is failing" }] }));
    await openChange();
    expect(screen.getByRole("button", { name: "Add to queue" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Urgent" })).toBeDisabled();
    expect(screen.getByText("Before it can queue")).toBeInTheDocument();
    expect(screen.getByText("test is failing")).toBeInTheDocument();
  });

  it("an ejected change says why, with the failing checks' output", async () => {
    api.change.mockResolvedValue(
      change({ queue: queued({ state: "ejected", position: null, error: "test failed on the batch", output: "test:\nFAIL parser.spec.ts" }) }),
    );
    await openChange();
    const note = screen.getByText(/Ejected from the merge queue: test failed on the batch/).closest("div")!;
    expect(note.querySelector("pre")).toHaveTextContent("FAIL parser.spec.ts");
    expect(note).toHaveTextContent("Revise it, then add it to the queue again.");
    expect(screen.getByText("ejected", { selector: "span" })).toBeInTheDocument();
  });

  it("an ejected change without output shows just the reason", async () => {
    api.change.mockResolvedValue(change({ queue: queued({ state: "ejected", position: null, error: "timed out after 600s" }) }));
    await openChange();
    const note = screen.getByText(/Ejected from the merge queue: timed out after 600s/).closest("div")!;
    expect(note.querySelector("pre")).toBeNull();
  });

  it("a conflict lists the files and the head to revise on", async () => {
    api.change.mockResolvedValue(change({ state: "conflict", conflict: { paths: ["src/app.ts", "README.md"], head: "cs_main0000000" } }));
    await openChange();
    const note = screen.getByText(/Didn't land: 2 files changed on main/).closest("div")!;
    expect(within(note).getAllByRole("listitem").map((li) => li.textContent)).toEqual(["src/app.ts", "README.md"]);
    expect(note).toHaveTextContent("Revise it on top of cs_main0000000 and land again.");
  });

  it("a conflict answer from Add to queue shows the conflict", async () => {
    api.enqueue.mockResolvedValue({ status: "conflict", error: null, paths: ["src/app.ts"], head: "cs_main0000000" });
    const { u } = await openChange();
    await u.click(screen.getByRole("button", { name: "Add to queue" }));
    expect(await screen.findByText(/Didn't land: 1 file changed on main/)).toBeInTheDocument();
    expect(screen.getByText("Conflicts in 1 file")).toBeInTheDocument();
  });
});
