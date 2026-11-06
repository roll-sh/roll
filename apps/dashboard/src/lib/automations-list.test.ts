import { describe, expect, it } from "vitest";
import { automation, automationRun, T } from "@/test/automations";
import { filterAutomations, liveRunOf, recentRunsOf } from "./automations-list";

const triage = automation();
const broken = automation({ id: "aut_2", name: "Fix a broken main", trigger: { type: "code", events: ["work"], repos: ["acme/roll"] } });
const hook = automation({ id: "aut_3", name: "Deploy hook", enabled: false, trigger: { type: "webhook" } as never, steps: [{ id: "post", kind: "slack.post", config: {}, when: [], retry: null, timeout_s: null }] });
const all = [triage, broken, hook];
const runs = [
  automationRun({ id: "arn_1", automation: "aut_1", status: "succeeded", created_at: T - 3000 }),
  automationRun({ id: "arn_2", automation: "aut_1", status: "running", created_at: T - 1000 }),
  automationRun({ id: "arn_3", automation: "aut_2", status: "failed", created_at: T - 2000 }),
];
const ctx = { runs, summary: (a: { name: string }) => (a.name === "Morning triage" ? "Weekdays at 09:00" : ""), projectsOf: (a: { id: string }) => (a.id === "aut_2" ? ["prj_roll"] : []) };
const names = (xs: { name: string }[]) => xs.map((x) => x.name);

describe("the automations list", () => {
  it("a row's live run and its last runs oldest first", () => {
    expect(liveRunOf("aut_1", runs)?.id).toBe("arn_2");
    expect(liveRunOf("aut_2", runs)).toBeNull();
    expect(recentRunsOf("aut_1", runs).map((r) => r.id)).toEqual(["arn_1", "arn_2"]);
    expect(recentRunsOf("aut_1", runs, 1).map((r) => r.id)).toEqual(["arn_2"]);
  });

  it("searches every word across name, trigger in words and step kinds", () => {
    expect(names(filterAutomations(all, "weekdays", {}, ctx))).toEqual(["Morning triage"]);
    expect(names(filterAutomations(all, "slack hook", {}, ctx))).toEqual(["Deploy hook"]);
    expect(names(filterAutomations(all, "  ", {}, ctx))).toEqual(names(all));
    expect(filterAutomations(all, "nothing like it", {}, ctx)).toEqual([]);
  });

  it("filters by status, trigger and project, together", () => {
    expect(names(filterAutomations(all, "", { status: "paused" }, ctx))).toEqual(["Deploy hook"]);
    expect(names(filterAutomations(all, "", { status: "on" }, ctx))).toEqual(["Morning triage", "Fix a broken main"]);
    expect(names(filterAutomations(all, "", { status: "failing" }, ctx))).toEqual(["Fix a broken main"]);
    expect(names(filterAutomations(all, "", { status: "running" }, ctx))).toEqual(["Morning triage"]);
    expect(names(filterAutomations(all, "", { trigger: "code" }, ctx))).toEqual(["Fix a broken main"]);
    expect(names(filterAutomations(all, "", { project: "prj_roll" }, ctx))).toEqual(["Fix a broken main"]);
    expect(filterAutomations(all, "", { trigger: "code", status: "paused" }, ctx)).toEqual([]);
  });

});
