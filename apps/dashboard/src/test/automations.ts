/** Automations fixtures for component tests: records, runs and an in-memory API. */
import { vi } from "vitest";
import type { Automation, AutomationRun, AutomationsApi, AutomationSpec, StepRun } from "@/lib/automations-api";

export const T = Date.now();

export function automation(over: Partial<Automation> = {}): Automation {
  return {
    id: "aut_1",
    name: "Morning triage",
    enabled: true,
    trigger: { type: "schedule", cron: "0 9 * * 1-5", timezone: "UTC" },
    filters: [],
    steps: [{ id: "tell", kind: "notify", config: { title: "Hi", level: "info", link: null }, when: [], retry: null, timeout_s: null }],
    concurrency: "parallel",
    rate_limit: null,
    run_as: null,
    revision: 1,
    cursor: 1,
    created_at: T,
    updated_at: T,
    created_by: "ada",
    secrets: [],
    webhook: null,
    next_at: null,
    last_run: null,
    ...over,
  };
}

export const stepRun = (id: string, kind: string, status: StepRun["status"], over: Partial<StepRun> = {}): StepRun => ({ id, kind, status, attempt: 1, started_at: T - 5000, finished_at: status === "succeeded" ? T - 1000 : null, next_at: null, output: null, error: null, log: [], ...over });

export function automationRun(over: Partial<AutomationRun> = {}): AutomationRun {
  return {
    id: "arn_1",
    automation: "aut_1",
    name: "Morning triage",
    status: "running",
    event: { id: "aev_1", source: "schedule", kind: "tick", summary: "Weekdays at 09:00", at: T - 6000 },
    steps: [stepRun("start", "agent.start", "succeeded"), stepRun("wait", "agent.wait", "waiting", { log: [{ at: T - 4000, level: "info", text: "Waiting for thr_1" }] })],
    threads: [{ machine: "mch_1", thread: "thr_1", step: "start" }],
    cursor: 5,
    created_at: T - 6000,
    started_at: T - 6000,
    finished_at: null,
    reason: null,
    ...over,
  };
}

export function fakeApi(list: Automation[] = [], runs: AutomationRun[] = []): AutomationsApi {
  const saved = (a: Automation) => ({ automation: a, webhook_secret: null });
  return {
    list: vi.fn(async () => list),
    get: vi.fn(async (_o: string, id: string) => list.find((a) => a.id === id)!),
    create: vi.fn(async (_o: string, spec: AutomationSpec) => saved(automation({ ...spec, id: "aut_new", cursor: 10 }))),
    update: vi.fn(async (_o: string, id: string, spec: AutomationSpec) => saved(automation({ ...spec, id, revision: 2, cursor: 11 }))),
    remove: vi.fn(async () => {}),
    enable: vi.fn(async (_o: string, id: string) => automation({ ...list.find((a) => a.id === id), enabled: true, cursor: 20 })),
    disable: vi.fn(async (_o: string, id: string) => automation({ ...list.find((a) => a.id === id), enabled: false, cursor: 20 })),
    run: vi.fn(async () => automationRun({ id: "arn_now", status: "queued" })),
    runsOf: vi.fn(async () => runs),
    runs: vi.fn(async () => runs),
    runDetail: vi.fn(async (_o: string, id: string) => ({ run: runs.find((r) => r.id === id)!, event: { id: "aev_1", source: "schedule", kind: "tick", key: "schedule:1", at: T, data: { at: T }, automation: "aut_1" } })),
    cancel: vi.fn(async (_o: string, id: string) => ({ ...runs.find((r) => r.id === id)!, status: "cancelled" as const, cursor: 30 })),
    retry: vi.fn(),
    setSecret: vi.fn(async () => {}),
    removeSecret: vi.fn(async () => {}),
    rotateWebhookSecret: vi.fn(),
    catalog: vi.fn(async () => ({ triggers: [], steps: [] })),
    test: vi.fn(async () => ({ matched: true, trigger: true, filters: [], steps: [{ id: "notify", runs: true, config: { title: "Hi there" }, error: null }] })),
  } as AutomationsApi;
}
