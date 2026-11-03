import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Command, Reply, Thread } from "@/lib/agents-api";
import { ChangeChip, changeTone, landingNote } from "./change";
import { ThreadNode } from "./nodes";
import { hunks } from "./request-card";
import { ThreadSession } from "./session";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";

const change = { repo: "acme/roll", number: 42, status: "blocked" as const, url: "https://roll.sh/acme/roll/changes/42", detail: "Waiting on 1 required check" };
const work = { id: "wi_7", repo: "acme/roll", number: 7, title: "Make the queue test deterministic" };
const inRepo = project({ repo: { owner: "acme", repo: "roll" } });
const worktree = { path: "/Users/ada/.roll/worktrees/prj_1/thr_1", branch: "roll/fix-the-flaky-test", base: "abc" };

describe("roll code on a thread", () => {
  it("the change chip says where the change is and links to it", () => {
    renderAgents(<ChangeChip change={{ ...change, status: "ready", detail: null }} />, agentsHarness());
    const chip = screen.getByRole("link", { name: "Change 42, ready" });
    expect(chip).toHaveAttribute("href", "https://roll.sh/acme/roll/changes/42");
    expect(chip).toHaveTextContent("#42ready");
    expect(changeTone("checking")).toBe("busy");
    expect(changeTone("queued")).toBe("busy");
    expect(changeTone("landed")).toBe("ok");
    expect(changeTone("failing")).toBe("bad");
    expect(changeTone("conflict")).toBe("bad");
    expect(changeTone("blocked")).toBe("quiet");
    expect(changeTone("closed")).toBe("quiet");
  });

  it("the thread card shows its change and work item", () => {
    const t = thread({ change, work });
    renderAgents(<ThreadNode thread={t} machine={machine()} />, agentsHarness({ entities: [mirrored("thread", t, 1)] }));
    const card = screen.getByRole("button", { name: "Thread Fix the flaky queue test" });
    expect(within(card).getByRole("link", { name: "Change 42, blocked" })).toBeInTheDocument();
    expect(within(card).getByRole("link", { name: /#7 Make the queue test deterministic/ })).toHaveAttribute("href", "/acme/roll/work");
  });

  it("the session header shows the change's detail and lets go of the work item", async () => {
    const t = thread({ change, work });
    const services = fakeServices();
    const h = agentsHarness({ entities: [mirrored("project", inRepo, 1), mirrored("thread", t, 2)], follow: ["thr_1"], ctx: { services } });
    renderAgents(<ThreadSession thread={t} machine={machine()} />, h);
    expect(screen.getByText("Waiting on 1 required check")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Let go of the work item" }));
    expect(services.releaseWork).toHaveBeenCalledWith("acme", "thr_1");
  });

  it("publishes and lands; a blocked land reads as a calm result with its blockers", async () => {
    const t: Thread = thread({ worktree });
    const reply = (c: Command): Reply =>
      c.type === "thread.publish"
        ? {
            type: "published",
            branch: "roll/fix-the-flaky-test",
            commit: "4b825dc642cb",
            change: 42,
            url: "https://roll.sh/acme/roll/changes/42",
            landing: { status: "blocked", message: "1 blocker", blockers: ["check `test` hasn't passed"] },
          }
        : { type: "ok" };
    const h = agentsHarness({ reply, entities: [mirrored("project", inRepo, 1), mirrored("thread", t, 2)], follow: ["thr_1"], ctx: { services: fakeServices() } });
    renderAgents(<ThreadSession thread={t} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Publish as a roll code change" }));
    const form = screen.getByRole("form", { name: "Publish" });
    expect(within(form).getByRole("textbox", { name: "Commit message" })).toHaveValue("Fix the flaky queue test");
    await userEvent.click(within(form).getByRole("checkbox", { name: "and land" }));
    await userEvent.click(within(form).getByRole("button", { name: "Publish" }));
    const done = await screen.findByRole("status", { name: "Published" });
    expect(h.command).toHaveBeenLastCalledWith("mch_1", expect.any(String), { type: "thread.publish", thread: "thr_1", message: "Fix the flaky queue test", land: true });
    expect(within(done).getByRole("link", { name: /change #42/ })).toHaveAttribute("href", "https://roll.sh/acme/roll/changes/42");
    expect(within(done).getByText(/Not landed yet/)).not.toHaveClass("text-danger");
    expect(within(done).getByRole("list", { name: "Blockers" })).toHaveTextContent("check `test` hasn't passed");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("a thread outside a worktree is offered a fork into one before publishing", async () => {
    const t = thread();
    const h = agentsHarness({ reply: () => ({ type: "created", id: "thr_fork" }), entities: [mirrored("project", inRepo, 1), mirrored("thread", t, 2)], follow: ["thr_1"] });
    renderAgents(<ThreadSession thread={t} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Publish as a roll code change" }));
    await userEvent.click(screen.getByRole("button", { name: "Fork into a worktree" }));
    expect(h.command).toHaveBeenLastCalledWith("mch_1", expect.any(String), { type: "thread.fork", thread: "thr_1", worktree: true });
    await waitFor(() => expect(h.setExpanded).toHaveBeenCalledWith("thr_fork", true));
  });

  it("claims the next work item of the project's repo, and says so calmly when there's none", async () => {
    const t = thread();
    const takeWork = vi.fn().mockResolvedValueOnce(null).mockResolvedValueOnce(work);
    const h = agentsHarness({ entities: [mirrored("project", inRepo, 1), mirrored("thread", t, 2)], follow: ["thr_1"], ctx: { services: fakeServices({ takeWork }) } });
    renderAgents(<ThreadSession thread={t} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Take a work item" }));
    await userEvent.click(screen.getByRole("button", { name: "Claim the next in acme/roll" }));
    expect(await screen.findByText("Nothing open to claim in acme/roll.")).toBeInTheDocument();
    expect(takeWork).toHaveBeenLastCalledWith("acme", "thr_1", { repo: "acme/roll" });
    const field = screen.getByRole("textbox", { name: "Work item" });
    await userEvent.type(field, "acme/roll#7");
    await userEvent.click(screen.getByRole("button", { name: "Take" }));
    expect(takeWork).toHaveBeenLastCalledWith("acme", "thr_1", { work: "acme/roll#7" });
    await waitFor(() => expect(screen.queryByRole("form", { name: "Take a work item" })).not.toBeInTheDocument());
  });

  it("full screen (a phone), the session is a dialog with its own close", async () => {
    const t = thread({ change });
    const h = agentsHarness({ entities: [mirrored("project", inRepo, 1), mirrored("thread", t, 2)], follow: ["thr_1"] });
    renderAgents(<ThreadSession thread={t} machine={machine()} fullscreen />, h);
    const sheet = screen.getByRole("dialog", { name: "Thread Fix the flaky queue test" });
    expect(within(sheet).getByRole("link", { name: "Change 42, blocked" })).toBeInTheDocument();
    expect(within(sheet).getByRole("textbox", { name: "Message" })).toBeInTheDocument();
    await userEvent.click(within(sheet).getByRole("button", { name: "Close" }));
    expect(h.setExpanded).toHaveBeenCalledWith("thr_1", false);
  });

  it("a fresh fork with no runs isn't stuck loading", () => {
    const t = thread({ run_count: 0 });
    renderAgents(<ThreadSession thread={t} machine={machine()} />, agentsHarness({ entities: [mirrored("thread", t, 1)], follow: ["thr_1"] }));
    expect(screen.queryByText("Loading the transcript…")).not.toBeInTheDocument();
    expect(screen.getByText(/No runs yet/)).toBeInTheDocument();
  });
});

describe("landing notes and diffs", () => {
  it("only a failed land is an error", () => {
    expect(landingNote({ status: "landed", message: "", blockers: [] }).tone).toBe("ok");
    expect(landingNote({ status: "queued", message: "", blockers: [] }).tone).toBe("busy");
    expect(landingNote({ status: "blocked", message: "", blockers: [] }).tone).toBe("quiet");
    expect(landingNote({ status: "conflict", message: "", blockers: [] }).tone).toBe("quiet");
    expect(landingNote({ status: "failed", message: "", blockers: [] }).tone).toBe("bad");
  });

  it("the diff viewer gets hunks without git's header lines", () => {
    const git = "diff --git a/ROLL_DEMO.md b/ROLL_DEMO.md\nnew file mode 100644\nindex 0000000..e69de29\n--- /dev/null\n+++ b/ROLL_DEMO.md\n@@ -0,0 +1 @@\n+hello\n";
    expect(hunks(git)).toBe("@@ -0,0 +1 @@\n+hello\n");
    expect(hunks("@@ -1 +1 @@\n-a\n+b")).toBe("@@ -1 +1 @@\n-a\n+b");
    expect(hunks("diff --git a/x b/x\nold mode 100644\nnew mode 100755\n")).toBeNull();
    expect(hunks(null)).toBeNull();
  });
});
