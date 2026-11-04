/**
 * The automations of the agents demo (`agents-demo.ts`'s owner): a few believable automations
 * and their runs, kept in memory behind the same shape as `createAutomationsApi`, so the
 * Automations view works signed out. Runs made with "Run now" move through their steps on
 * timers, and every change is announced through `on` as the stream would.
 */
import type { StreamEvent } from "./agents-api";
import type {
  Automation,
  AutomationId,
  AutomationRun,
  AutomationSaved,
  AutomationSpec,
  AutomationsApi,
  Catalog,
  RunDetail,
  RunNow,
  RunsQuery,
  Step,
  StepRun,
  TestRequest,
  TestResult,
} from "./automations-api";
import { isRunFinal } from "./automations-api";

const MIN = 60_000;
const HOUR = 60 * MIN;

function step(id: string, kind: string, config: unknown, extra: Partial<Step> = {}): Step {
  return { id, kind, config, when: [], retry: null, timeout_s: null, ...extra };
}

const MBP = "mch_demo_ada";

/** The demo's automations, as of `now`. */
export function demoAutomationSpecs(): (AutomationSpec & { id: AutomationId })[] {
  const base = { enabled: true, filters: [], concurrency: "parallel" as const, rate_limit: null, run_as: null };
  return [
    {
      ...base,
      id: "aut_demo_checks",
      name: "Fix failing checks",
      description: "When a change's checks fail, an agent in a fresh sandbox fixes them and pushes to the same branch.",
      trigger: { type: "code", events: ["check"], repos: ["acme/roll"] },
      filters: [{ field: "event.data.check.status", op: "equals", value: "failed" }],
      concurrency: "cancel_previous",
      steps: [
        step("start", "agent.start", {
          target: { type: "sandbox", size: "standard" },
          project: { type: "repo", repo: "acme/roll", path: null },
          provider: "codex",
          model: "gpt-5.5",
          effort: "high",
          runtime_mode: "auto_edit",
          interaction_mode: null,
          worktree: true,
          title: "Fix {{event.data.check.name}} on #{{event.data.change.number}}",
          prompt: "The check {{event.data.check.name}} fails on {{event.data.change.url}}. Find out why and fix it.",
        }),
        step("wait", "agent.wait", { thread: "{{steps.start.thread}}", until: ["done", "failed"], timeout_s: 3600 }),
        step("publish", "agent.publish", { thread: null, message: "Fix {{event.data.check.name}}", land: false }, { when: [{ field: "steps.wait.outcome", op: "equals", value: "done" }] }),
        step("tell", "notify", { title: "Checks fixed on #{{event.data.change.number}}", level: "success", link: "{{steps.publish.url}}" }),
      ],
    },
    {
      ...base,
      id: "aut_demo_triage",
      name: "Morning triage",
      description: "Weekdays at 9:00, an orchestrator reads yesterday's issues and opens work items.",
      trigger: { type: "schedule", cron: "0 9 * * 1-5", timezone: "Europe/Budapest" },
      concurrency: "skip",
      steps: [
        step("start", "agent.start", {
          target: { type: "machine", machine: "ada-mbp" },
          project: { type: "id", id: "prj_demo_roll" },
          provider: "claude",
          model: null,
          effort: null,
          runtime_mode: "ask",
          interaction_mode: "plan",
          worktree: false,
          title: "Triage {{event.data.at}}",
          prompt: "Go through the issues opened since yesterday, group them, and open a work item for each real bug.",
          orchestrator: true,
        }),
        step("answer", "agent.respond", { thread: null, rules: [{ action: "command", command: "gh *", files: null, tool: null, decision: "accept" }], otherwise: null, message: null }),
      ],
    },
    {
      ...base,
      id: "aut_demo_deploy",
      name: "Tell Slack when a deploy lands",
      description: "Every landing on main posts to the release channel.",
      trigger: { type: "code", events: ["landed"], repos: [] },
      steps: [
        step("post", "webhook", {
          url: "https://hooks.slack.com/services/T000/B000/XXXX",
          method: "POST",
          body: { text: "{{event.data.repo}}: {{event.data.change.title}} landed" },
          secret: null,
          ok_statuses: [],
        }),
      ],
    },
    {
      ...base,
      id: "aut_demo_release",
      name: "Cut a release",
      description: "By hand: an agent bumps the version, writes the changelog and queues the change to land.",
      trigger: { type: "manual" },
      allow_agents: true,
      steps: [
        step("start", "agent.start", {
          target: { type: "any_online" },
          project: { type: "id", id: "prj_demo_roll" },
          provider: "codex",
          model: null,
          effort: null,
          runtime_mode: "full_access",
          interaction_mode: null,
          worktree: true,
          title: "Release {{event.data.input.version | default:\"next\"}}",
          prompt: "Bump the version to {{event.data.input.version | default:\"the next minor\"}}, update CHANGELOG.md from the landed changes, and run the tests.",
        }),
        step("wait", "agent.wait", { thread: null, until: ["done", "failed"], timeout_s: 1800 }),
        step("ship", "agent.publish", { thread: null, message: "Release {{event.data.input.version}}", land: true }),
        step("tell", "notify", { title: "Release queued", body: "{{steps.ship.landing.message}}", level: "success", link: "{{steps.ship.url}}" }),
      ],
    },
  ];
}

function stepRun(s: Step, status: StepRun["status"], at: number, output: unknown = null, extra: Partial<StepRun> = {}): StepRun {
  const done = status === "succeeded" || status === "failed" || status === "skipped";
  return {
    id: s.id,
    kind: s.kind,
    status,
    attempt: status === "pending" || status === "skipped" ? 0 : 1,
    started_at: status === "pending" ? null : at,
    finished_at: done ? at + 30_000 : null,
    next_at: null,
    output,
    error: null,
    log: [],
    ...extra,
  };
}

/** The demo's run history, newest first. */
export function demoRuns(now: number): AutomationRun[] {
  const [checks, triage, deploy] = demoAutomationSpecs();
  const run = (n: number, a: (typeof checks), status: AutomationRun["status"], at: number, steps: StepRun[], summary: string, extra: Partial<AutomationRun> = {}): AutomationRun => ({
    id: `arn_demo_${n}`,
    automation: a.id,
    name: a.name,
    status,
    event: { id: `aev_demo_${n}`, source: a.trigger.type, kind: a.trigger.type === "schedule" ? "tick" : a.trigger.type === "code" ? "check" : "run", summary, at },
    steps,
    threads: [],
    cursor: 0,
    created_at: at,
    started_at: at,
    finished_at: isRunFinal(status) ? at + 4 * MIN : null,
    reason: null,
    ...extra,
  });
  const out = { machine: "mch_demo_sbx", project: "prj_demo_roll", thread: "thr_demo_fix", sandbox: "sbx_demo" };
  return [
    run(
      1,
      checks,
      "waiting",
      now - 6 * MIN,
      [stepRun(checks.steps[0], "succeeded", now - 6 * MIN, out), stepRun(checks.steps[1], "waiting", now - 5 * MIN, null, { next_at: now + 25_000 }), stepRun(checks.steps[2], "pending", 0), stepRun(checks.steps[3], "pending", 0)],
      "acme/roll#142: check test failed",
      { threads: [{ machine: "mch_demo_sbx", thread: "thr_demo_fix", step: "start" }] },
    ),
    run(2, deploy, "succeeded", now - 2 * HOUR, [stepRun(deploy.steps[0], "succeeded", now - 2 * HOUR, { status: 200, body: "ok" })], "acme/roll#139: landed"),
    run(
      3,
      deploy,
      "failed",
      now - 5 * HOUR,
      [
        stepRun(deploy.steps[0], "failed", now - 5 * HOUR, null, {
          attempt: 3,
          error: "hooks.slack.com answered 503 (after 3 attempts)",
          log: [{ at: now - 5 * HOUR, level: "warn", text: "attempt 1 failed: hooks.slack.com answered 503; trying again in 10 s" }],
        }),
      ],
      "acme/roll#137: landed",
      { reason: "step post failed: hooks.slack.com answered 503 (after 3 attempts)" },
    ),
    run(4, triage, "succeeded", now - 26 * HOUR, [stepRun(triage.steps[0], "succeeded", now - 26 * HOUR, { ...out, machine: MBP, thread: "thr_demo_triage" }), stepRun(triage.steps[1], "succeeded", now - 26 * HOUR, { answered: ["req_1"], left: [] })], "Scheduled for 09:00"),
    run(5, triage, "skipped", now - 50 * HOUR, triage.steps.map((s) => stepRun(s, "skipped", 0)), "Scheduled for 09:00", { reason: "another run was going" }),
  ];
}

export const DEMO_CATALOG: Catalog = {
  triggers: [
    { source: "manual", title: "By hand", description: "Run it from the dashboard, the CLI or MCP.", events: [{ kind: "run", title: "Run", fields: ["input", "by"], sample: null }], config: null },
    { source: "schedule", title: "On a schedule", description: "A cron expression in a time zone.", events: [{ kind: "tick", title: "Tick", fields: ["at"], sample: null }], config: null },
    { source: "interval", title: "Every so often", description: "Every so many seconds.", events: [{ kind: "tick", title: "Tick", fields: ["at"], sample: null }], config: null },
    { source: "webhook", title: "A webhook", description: "A signed POST to the automation's URL.", events: [{ kind: "post", title: "Post", fields: ["body", "headers"], sample: null }], config: null },
    { source: "code", title: "roll code", description: "Changes, checks, reviews, the queue, landings.", events: ["landed", "change", "check", "review", "queue", "work"].map((k) => ({ kind: k, title: k, fields: ["repo", "change.number", "change.title"], sample: null })), config: null },
    { source: "agent", title: "Agents", description: "Threads finishing, failing, waiting on you.", events: ["thread.done", "thread.failed", "thread.waiting", "request.opened"].map((k) => ({ kind: k, title: k, fields: ["thread.title", "run.status"], sample: null })), config: null },
  ],
  steps: ["agent.start", "agent.wait", "agent.message", "agent.respond", "agent.publish", "agent.script", "agent.settle", "webhook", "notify", "delay"].map((k) => ({
    kind: k,
    title: k,
    description: "",
    config: { type: "object" },
    output: k === "agent.start" ? ["machine", "project", "thread", "sandbox"] : [],
    integration: null,
  })),
};

/**
 * The demo's automations as an `AutomationsApi`. `on` gets the stream events a real relay would
 * send (the demo owner forwards them to the canvas).
 */
export class DemoAutomations implements AutomationsApi {
  private automations = new Map<AutomationId, Automation>();
  private history: AutomationRun[];
  private cursor = 1000;
  private timers = new Set<ReturnType<typeof setTimeout>>();

  constructor(
    private on: (ev: StreamEvent) => void = () => {},
    private speed = 1,
    now = Date.now(),
  ) {
    this.history = demoRuns(now);
    for (const s of demoAutomationSpecs()) {
      const { id, ...spec } = s;
      const last = this.history.find((r) => r.automation === id);
      this.automations.set(id, {
        ...spec,
        id,
        revision: 1,
        cursor: this.cursor++,
        created_at: now - 30 * 24 * HOUR,
        updated_at: now - 3 * 24 * HOUR,
        created_by: "acme",
        secrets: spec.trigger.type === "code" && id === "aut_demo_deploy" ? ["SLACK_WEBHOOK"] : [],
        webhook: null,
        next_at: spec.trigger.type === "schedule" ? now + 14 * HOUR : null,
        last_run: last ? { id: last.id, status: last.status, at: last.created_at } : null,
      });
    }
  }

  /** Stop the timers of runs going on. */
  stop() {
    this.timers.forEach(clearTimeout);
    this.timers.clear();
  }

  private find(id: AutomationId): Automation {
    const a = this.automations.get(id);
    if (!a) throw new Error(`automation ${id} not found`);
    return a;
  }

  private publishRun(r: AutomationRun) {
    r.cursor = this.cursor++;
    const a = this.automations.get(r.automation);
    if (a) a.last_run = { id: r.id, status: r.status, at: r.created_at };
    this.on({ type: "automation_run", cursor: r.cursor, run: structuredClone(r) });
  }

  private save(spec: AutomationSpec, id: AutomationId, prev?: Automation): AutomationSaved {
    const now = Date.now();
    const a: Automation = {
      ...spec,
      id,
      revision: (prev?.revision ?? 0) + 1,
      cursor: this.cursor++,
      created_at: prev?.created_at ?? now,
      updated_at: now,
      created_by: prev?.created_by ?? "acme",
      secrets: prev?.secrets ?? [],
      webhook: spec.trigger.type === "webhook" ? { url: `https://api.roll.sh/v1/code/agents/acme/automations/${id}/webhook`, secret_hint: "d3m0" } : null,
      next_at: spec.trigger.type === "schedule" || spec.trigger.type === "interval" ? now + HOUR : null,
      last_run: prev?.last_run ?? null,
    };
    this.automations.set(id, a);
    this.on({ type: "automation", cursor: a.cursor, automation: structuredClone(a) });
    return { automation: structuredClone(a), webhook_secret: spec.trigger.type === "webhook" && !prev?.webhook ? "rlh_demo_shown_once" : null };
  }

  list = async () => [...this.automations.values()].map((a) => structuredClone(a));
  get = async (_owner: string, id: AutomationId) => structuredClone(this.find(id));
  create = async (_owner: string, spec: AutomationSpec) => this.save(spec, `aut_demo_${Math.random().toString(36).slice(2, 10)}`);
  update = async (_owner: string, id: AutomationId, spec: AutomationSpec, revision?: number) => {
    const prev = this.find(id);
    if (revision !== undefined && revision !== prev.revision) throw new Error(`the automation changed (it is at revision ${prev.revision})`);
    return this.save(spec, id, prev);
  };
  remove = async (_owner: string, id: AutomationId) => {
    this.automations.delete(id);
    this.on({ type: "automation_removed", cursor: this.cursor++, automation: id });
  };
  enable = async (_owner: string, id: AutomationId) => this.save({ ...this.find(id), enabled: true }, id, this.find(id)).automation;
  disable = async (_owner: string, id: AutomationId) => this.save({ ...this.find(id), enabled: false }, id, this.find(id)).automation;

  run = async (_owner: string, id: AutomationId, req: Partial<RunNow> = {}) => {
    const a = this.find(id);
    const now = Date.now();
    const r: AutomationRun = {
      id: `arn_demo_${Math.random().toString(36).slice(2, 10)}`,
      automation: id,
      name: a.name,
      status: "running",
      event: { id: `aev_demo_${now}`, source: "manual", kind: "run", summary: "Run by acme", at: now },
      steps: a.steps.map((s) => stepRun(s, "pending", 0)),
      threads: [],
      cursor: 0,
      created_at: now,
      started_at: now,
      finished_at: null,
      reason: null,
    };
    void req;
    this.history.unshift(r);
    this.publishRun(r);
    this.advance(r, 0);
    return structuredClone(r);
  };

  /** Move a demo run through its steps: each one runs for a moment and succeeds. */
  private advance(r: AutomationRun, i: number) {
    if (r.status === "cancelled") return;
    const s = r.steps[i];
    if (!s) {
      r.status = "succeeded";
      r.finished_at = Date.now();
      this.publishRun(r);
      return;
    }
    s.status = s.kind === "agent.wait" ? "waiting" : "running";
    s.attempt = 1;
    s.started_at = Date.now();
    r.status = s.status === "waiting" ? "waiting" : "running";
    this.publishRun(r);
    const t = setTimeout(
      () => {
        this.timers.delete(t);
        if (r.status === "cancelled") return;
        s.status = "succeeded";
        s.finished_at = Date.now();
        if (s.kind === "agent.start") {
          s.output = { machine: MBP, project: "prj_demo_roll", thread: `thr_demo_${r.id.slice(-6)}` };
          r.threads.push({ machine: MBP, thread: `thr_demo_${r.id.slice(-6)}`, step: s.id });
        }
        this.advance(r, i + 1);
      },
      (s.kind === "agent.wait" ? 6000 : 1200) / this.speed,
    );
    this.timers.add(t);
  }

  runsOf = async (_owner: string, id: AutomationId, q: RunsQuery = {}) => this.select(q, id);
  runs = async (_owner: string, q: RunsQuery & { automation?: AutomationId } = {}) => this.select(q, q.automation);

  private select(q: RunsQuery, automation?: AutomationId): AutomationRun[] {
    let out = this.history.filter((r) => (!automation || r.automation === automation) && (!q.status || r.status === q.status));
    if (q.before) {
      const i = out.findIndex((r) => r.id === q.before);
      out = i >= 0 ? out.slice(i + 1) : out;
    }
    return out.slice(0, q.limit ?? 50).map((r) => structuredClone(r));
  }

  runDetail = async (_owner: string, run: string): Promise<RunDetail> => {
    const r = this.history.find((x) => x.id === run);
    if (!r) throw new Error(`run ${run} not found`);
    return { run: structuredClone(r), event: { id: r.event.id, source: r.event.source, kind: r.event.kind, key: `${r.event.source}:${r.event.id}`, at: r.event.at, data: {}, automation: r.automation } };
  };

  cancel = async (_owner: string, run: string) => {
    const r = this.history.find((x) => x.id === run);
    if (!r) throw new Error(`run ${run} not found`);
    if (!isRunFinal(r.status)) {
      r.status = "cancelled";
      r.finished_at = Date.now();
      r.reason = "cancelled";
      for (const s of r.steps) if (!["succeeded", "failed", "skipped"].includes(s.status)) s.status = "cancelled";
      this.publishRun(r);
    }
    return structuredClone(r);
  };

  retry = async (_owner: string, run: string) => {
    const r = this.history.find((x) => x.id === run);
    if (!r || (r.status !== "failed" && r.status !== "cancelled")) throw new Error("only a failed or cancelled run can be retried");
    const i = r.steps.findIndex((s) => s.status === "failed" || s.status === "cancelled");
    for (const s of r.steps.slice(i)) Object.assign(s, { status: "pending", attempt: 0, error: null, finished_at: null });
    r.status = "running";
    r.reason = null;
    r.finished_at = null;
    this.advance(r, Math.max(0, i));
    return structuredClone(r);
  };

  setSecret = async (_owner: string, id: AutomationId, name: string) => {
    const a = this.find(id);
    if (!a.secrets.includes(name)) a.secrets = [...a.secrets, name].sort();
  };
  removeSecret = async (_owner: string, id: AutomationId, name: string) => {
    const a = this.find(id);
    a.secrets = a.secrets.filter((s) => s !== name);
  };
  rotateWebhookSecret = async (_owner: string, id: AutomationId) => ({ automation: structuredClone(this.find(id)), webhook_secret: "rlh_demo_rotated" });
  catalog = async () => structuredClone(DEMO_CATALOG);
  test = async (_owner: string, req: TestRequest): Promise<TestResult> => ({
    matched: req.automation.trigger.type === req.event.source,
    trigger: req.automation.trigger.type === req.event.source,
    filters: req.automation.filters.map(() => true),
    steps: req.automation.steps.map((s) => ({ id: s.id, runs: true, config: s.config, error: null })),
  });
}
