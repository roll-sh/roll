import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Command, Reply, Thread } from "@/lib/agents-api";
import { hunks } from "./request-card";
import { Conversation } from "./session";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";

const work = { id: "wi_7", repo: "acme/roll", number: 7, title: "Make the queue test deterministic" };
const inRepo = project({ repo: { owner: "acme", repo: "roll" } });
const worktree = { path: "/Users/ada/.roll/worktrees/prj_1/thr_1", branch: "roll/fix-the-flaky-test", base: "abc", removed_at: null };

describe("roll code on a thread", () => {
  it("publishes and lands; a blocked land is a result with its blockers, not an error", async () => {
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
    renderAgents(<Conversation thread={t} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "More" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Publish as a change" }));
    const form = screen.getByRole("form", { name: "Publish" });
    await userEvent.click(within(form).getByRole("switch", { name: "and land" }));
    await userEvent.click(within(form).getByRole("button", { name: "Publish" }));
    const done = await screen.findByRole("status", { name: "Published" });
    expect(h.command).toHaveBeenLastCalledWith("mch_1", expect.any(String), { type: "thread.publish", thread: "thr_1", message: "Fix the flaky queue test", land: true });
    expect(within(done).getByRole("link", { name: /change #42/ })).toHaveAttribute("href", "/dashboard/code/acme/roll/changes/42");
    expect(within(done).getByRole("list", { name: "Blockers" })).toHaveTextContent("check `test` hasn't passed");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("a checkout that isn't a roll repo commits and pushes to its origin, without landing", async () => {
    const t: Thread = thread({ worktree });
    const reply = (c: Command): Reply => (c.type === "thread.publish" ? { type: "published", branch: "roll/fix-the-flaky-test", commit: "4b825dc642cb", change: null, url: null, landing: null } : { type: "ok" });
    const github = project({ git: { branch: "main", head: "f35f34cf", remote: "https://github.com/acme/app.git", dirty: false } });
    const h = agentsHarness({ reply, entities: [mirrored("project", github, 1), mirrored("thread", t, 2)], follow: ["thr_1"] });
    renderAgents(<Conversation thread={t} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "More" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Commit and push to origin" }));
    const form = screen.getByRole("form", { name: "Publish" });
    expect(within(form).queryByRole("switch", { name: "and land" })).not.toBeInTheDocument();
    await userEvent.click(within(form).getByRole("button", { name: "Commit and push" }));
    await screen.findByRole("status", { name: "Published" });
    expect(h.command).toHaveBeenLastCalledWith("mch_1", expect.any(String), { type: "thread.publish", thread: "thr_1", message: "Fix the flaky queue test", land: false });
  });

  it("claims the next work item of the project's repo, or the one named", async () => {
    const t = thread();
    const takeWork = vi.fn().mockResolvedValueOnce(null).mockResolvedValueOnce(work);
    const h = agentsHarness({ entities: [mirrored("project", inRepo, 1), mirrored("thread", t, 2)], follow: ["thr_1"], ctx: { services: fakeServices({ takeWork }) } });
    renderAgents(<Conversation thread={t} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "More" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Take a work item" }));
    await userEvent.click(screen.getByRole("button", { name: "Claim the next in acme/roll" }));
    expect(await screen.findByText("Nothing open to claim in acme/roll.")).toBeInTheDocument();
    expect(takeWork).toHaveBeenLastCalledWith("acme", "thr_1", { repo: "acme/roll" });
    const field = screen.getByRole("textbox", { name: "Work item" });
    await userEvent.type(field, "acme/roll#7");
    await userEvent.click(screen.getByRole("button", { name: "Take" }));
    expect(takeWork).toHaveBeenLastCalledWith("acme", "thr_1", { work: "acme/roll#7" });
    await waitFor(() => expect(screen.queryByRole("form", { name: "Take a work item" })).not.toBeInTheDocument());
  });

});

describe("diffs", () => {
  it("the diff viewer gets hunks without git's header lines", () => {
    const git = "diff --git a/ROLL_DEMO.md b/ROLL_DEMO.md\nnew file mode 100644\nindex 0000000..e69de29\n--- /dev/null\n+++ b/ROLL_DEMO.md\n@@ -0,0 +1 @@\n+hello\n";
    expect(hunks(git)).toBe("@@ -0,0 +1 @@\n+hello\n");
    expect(hunks("@@ -1 +1 @@\n-a\n+b")).toBe("@@ -1 +1 @@\n-a\n+b");
    expect(hunks("diff --git a/x b/x\nold mode 100644\nnew mode 100755\n")).toBeNull();
    expect(hunks(null)).toBeNull();
  });
});
