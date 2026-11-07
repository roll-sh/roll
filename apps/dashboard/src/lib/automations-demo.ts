/**
 * The demo owner's automations (`/dashboard?demo#agents/automations`): `AutomationsApi` answered in
 * memory, with believable automations and a week of runs, and runs that really go: an
 * `agent.start` step starts a thread on the demo canvas, `agent.wait` follows it, a `notify` step
 * sends a notice. Every change is an `automation`/`automation_run` event on the demo's stream, as
 * the relay sends them. The template filling and filters here are a small stand-in for the
 * server's (`template.rs`, `filter.rs`), enough for "Test" to show something true-looking.
 */

import { withChatops } from "./chatops-catalog";
import type { Command, Machine, MachineId, Reply, StreamEvent, ThreadStatus } from "./agents-api";
import {
  isRunFinal,
  type Automation,
  type AutomationEvent as WireEvent,
  type AutomationRun,
  type AutomationSpec,
  type AutomationsApi,
  type Catalog,
  type Filter,
  type Notice,
  type RunState,
  type Step,
  type StepRun,
  type TestResult,
} from "./automations-api";
import { ApiError } from "./code-api";
import { LINEAR_CATALOG } from "./linear-templates";
import { BUILT_IN_STEPS, BUILT_IN_TRIGGERS, ev, triggerSource, withBuiltIns } from "./automations-ui";

/** What the demo owner lends its automations. */
export interface DemoHost {
  emit(ev: StreamEvent): void;
  /** The next stream cursor. */
  next(): number;
  machines(): Machine[];
  projects(): { id: string; name: string; machine: MachineId; repo: string | null }[];
  command(machine: MachineId, c: Command): Promise<Reply>;
  threadStatus(thread: string): ThreadStatus | undefined;
  after(ms: number, fn: () => void): void;
  /** The first-run demo (`?demo=empty`): no automations yet either. */
  empty?(): boolean;
}

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

let seq = 0;
const nid = (prefix: string) => `${prefix}${Date.now().toString(36)}${(++seq).toString(36).padStart(4, "0")}`;

const step = (id: string, kind: string, config: Record<string, unknown>, over: Partial<Step> = {}): Step => ({ id, kind, config, when: [], retry: null, timeout_s: null, ...over });

const agentStart = (over: Record<string, unknown>) => ({
  target: null,
  project: null,
  provider: "claude",
  model: "claude-opus-5-5",
  effort: "high",
  runtime_mode: "full_access",
  interaction_mode: null,
  worktree: true,
  title: null,
  prompt: "",
  ...over,
});

/** The demo catalog: the built-ins, plus integrations (the demo has Linear, Slack and GitHub
 * connected, as its integration cards say; Discord isn't). Linear's entries are the server's
 * (`linear-catalog.json`). */
const CATALOG: Catalog = withChatops(withBuiltIns({
  triggers: [...BUILT_IN_TRIGGERS, ...LINEAR_CATALOG.triggers],
  steps: [...LINEAR_CATALOG.steps],
}), (i) => i === "slack" || i === "github");

export class DemoAutomations {
  private automations = new Map<string, Automation>();
  private runs = new Map<string, AutomationRun>();
  private events = new Map<string, WireEvent>();
  /** Notices kept as the server keeps them (newest first). */
  private notices: Notice[] = [];
  private seeded = false;

  constructor(private host: DemoHost) {}

  // ── records ──

  private putAutomation(a: Automation): Automation {
    const next = { ...a, cursor: this.host.next(), updated_at: Date.now() };
    this.automations.set(a.id, next);
    this.host.emit({ type: "automation", cursor: next.cursor, automation: next });
    return next;
  }

  private putRun(r: AutomationRun): AutomationRun {
    const next = { ...r, cursor: this.host.next() };
    this.runs.set(r.id, next);
    this.host.emit({ type: "automation_run", cursor: next.cursor, run: next });
    return next;
  }

  private patchStep(rid: string, sid: string, patch: Partial<StepRun>, run: Partial<AutomationRun> = {}) {
    const r = this.runs.get(rid);
    if (!r || isRunFinal(r.status)) return;
    this.putRun({ ...r, ...run, steps: r.steps.map((s) => (s.id === sid ? { ...s, ...patch } : s)) });
  }

  private log(rid: string, sid: string, text: string, level: "info" | "warn" | "error" = "info") {
    const r = this.runs.get(rid);
    const s = r?.steps.find((x) => x.id === sid);
    if (!s) return;
    this.patchStep(rid, sid, { log: [...s.log, { at: Date.now(), level, text }] });
  }

  private must(id: string) {
    this.seed();
    const a = this.automations.get(id);
    if (!a) throw new ApiError("no such automation", 404, null);
    return a;
  }

  // ── the seed: a believable week ──

  private seed() {
    if (this.seeded) return;
    this.seeded = true;
    if (this.host.empty?.()) return;
    const t = Date.now();
    const machines = this.host.machines();
    const mbp = machines.find((m) => !m.sandbox) ?? machines[0];
    const projects = this.host.projects();
    const roll = projects.find((p) => p.name === "roll") ?? projects[0];
    const make = (id: string, spec: AutomationSpec, at: number, over: Partial<Automation> = {}): Automation => ({
      ...spec,
      id,
      revision: 1,
      cursor: this.host.next(),
      created_at: at,
      updated_at: at,
      created_by: "ada",
      secrets: [],
      webhook: null,
      next_at: null,
      last_run: null,
      ...over,
    });
    const base = { enabled: true, filters: [] as Filter[], concurrency: "parallel" as const, rate_limit: null, run_as: null };
    const list: Automation[] = [
      make(
        "aut_demo_mainfix",
        {
          ...base,
          name: "Fix a broken main",
          description: "Main is red: find the cause, fix it, queue the fix.",
          trigger: { type: "code", events: ["work"], repos: ["acme/roll"] },
          filters: [
            { field: "event.data.labels", op: "has_label", value: "broken-main" },
            { field: "event.data.state", op: "equals", value: "open" },
          ],
          steps: [
            step("fix", "agent.start", agentStart({ target: { type: "sandbox", size: "standard" }, project: { type: "repo", repo: "acme/roll", path: null }, title: "Fix main: {{ event.data.title }}", prompt: "Main is broken: {{ event.data.title }}.\n\nFind the commit that broke it, fix the cause, and make the checks pass." }), { retry: { attempts: 2, backoff_s: 60 } }),
            step("wait", "agent.wait", { thread: "{{ steps.fix.thread }}", until: ["done", "failed"], timeout_s: 5400, on_timeout: "fail" }),
            step("land", "agent.publish", { thread: "{{ steps.fix.thread }}", message: "Fix main: {{ event.data.title }}", land: true }),
          ],
          concurrency: "cancel_previous",
        },
        t - 12 * DAY,
      ),
      make(
        "aut_demo_triage",
        {
          ...base,
          name: "Morning triage",
          description: "Labels and prioritises yesterday's new work items.",
          trigger: { type: "schedule", cron: "0 9 * * 1-5", timezone: "Europe/Budapest" },
          steps: [
            step("triage", "agent.start", agentStart({ target: { type: "project_machine" }, project: roll ? { type: "id", id: roll.id } : null, provider: "codex", model: "gpt-5.5-codex", effort: "medium", runtime_mode: "auto_edit", worktree: false, title: "Triage", prompt: "Read the work items opened since the last triage. Label, prioritise, close duplicates. Summarise what needs a person." })),
            step("wait", "agent.wait", { thread: "{{ steps.triage.thread }}", until: ["done", "failed"], timeout_s: 3600, on_timeout: "continue" }),
            step("tell", "notify", { title: "Triage is done", body: "{{ steps.wait.message | truncate:280 }}", level: "success", link: null }),
          ],
          concurrency: "skip",
        },
        t - 20 * DAY,
        { next_at: nextWeekday9(t) },
      ),
      make(
        "aut_demo_publish",
        {
          ...base,
          name: "Publish finished work",
          description: "Done threads in a worktree become changes in the merge queue.",
          trigger: { type: "agent", events: ["thread.done"] },
          filters: [{ field: "event.data.project", op: "equals", value: roll?.id ?? "prj_demo_roll" }],
          steps: [
            step("publish", "agent.publish", { thread: "{{ event.data.thread.id }}", land: true }),
            step("settle", "agent.settle", { thread: "{{ event.data.thread.id }}", action: "settle" }),
          ],
        },
        t - 9 * DAY,
      ),
      make(
        "aut_demo_run",
        {
          ...base,
          name: `Run on ${mbp?.name ?? "my machine"}`,
          description: "An agent in a fresh worktree, started by hand.",
          trigger: { type: "manual" },
          steps: [
            step("start", "agent.start", agentStart({ target: mbp ? { type: "machine", machine: mbp.id } : { type: "any_online" }, project: roll ? { type: "id", id: roll.id } : null, prompt: '{{ event.data.input.prompt | default:"Pick an open work item and fix it." }}' })),
            step("wait", "agent.wait", { thread: "{{ steps.start.thread }}", until: ["done", "failed", "waiting"], timeout_s: 7200, on_timeout: "continue" }),
            step("tell", "notify", { title: "{{ automation.name }}: {{ steps.wait.outcome }}", body: "{{ steps.wait.message | truncate:200 }}", level: "info", link: null }),
          ],
          allow_agents: true,
        },
        t - 3 * DAY,
      ),
      make(
        "aut_demo_approve",
        {
          ...base,
          name: "Approve tests and builds",
          description: "Yes to test, build and lint commands.",
          trigger: { type: "agent", events: ["request.opened"] },
          filters: [{ field: "event.data.request.body.action", op: "equals", value: "command" }],
          steps: [
            step("answer", "agent.respond", {
              thread: "{{ event.data.thread.id }}",
              rules: [
                { action: "command", command: "cargo test*", files: null, tool: null, decision: "accept" },
                { action: "command", command: "pnpm test*", files: null, tool: null, decision: "accept" },
              ],
              otherwise: null,
              message: null,
            }),
          ],
        },
        t - 15 * DAY,
      ),
      make(
        "aut_demo_deploy",
        {
          ...base,
          enabled: false,
          name: "Deploy hook from CI",
          description: "CI posts a failed deploy; an agent reads the log and opens a fix.",
          trigger: { type: "webhook" },
          steps: [
            step("start", "agent.start", agentStart({ target: { type: "sandbox", size: "large", keep: false }, project: { type: "repo", repo: "acme/infra", path: null }, title: "Deploy failed: {{ event.data.body.service }}", prompt: "The deploy of {{ event.data.body.service }} failed:\n\n{{ event.data.body.log | lines:80 }}\n\nFind out why and open a fix." })),
            step("ping", "webhook", { url: "https://hooks.acme.dev/deploys", method: "POST", headers: { Authorization: "Bearer {{ secrets.ACME_TOKEN }}" }, body: { thread: "{{ steps.start.thread }}" }, secret: null, ok_statuses: [] }),
          ],
        },
        t - 30 * DAY,
        { secrets: ["ACME_TOKEN"], webhook: { url: "https://roll.sh/v1/code/agents/demo/automations/aut_demo_deploy/hook", secret_hint: "q7Zk" } },
      ),
    ];
    for (const a of list) this.automations.set(a.id, a);

    // A week of runs.
    const past = (a: Automation, at: number, status: RunState, summary: string, opts: { failAt?: number; error?: string; threads?: { thread: string; machine: string; step: string }[]; skipped?: string } = {}) => {
      const rid = nid("arn_demo");
      let clock = at + 400;
      const steps: StepRun[] = a.steps.map((s, i) => {
        const len = s.kind === "agent.wait" ? 6 * MIN + (Math.floor(at / 1000) % 9) * MIN : s.kind === "agent.start" ? 2400 : 900 + (i * 377) % 1400;
        const failed = opts.failAt === i;
        const after = opts.failAt !== undefined && i > opts.failAt;
        const start = clock;
        clock += len;
        const st: StepRun["status"] = status === "skipped" ? "skipped" : failed ? "failed" : after ? "skipped" : "succeeded";
        return {
          id: s.id,
          kind: s.kind,
          status: st,
          attempt: failed && s.retry ? s.retry.attempts : st === "skipped" ? 0 : 1,
          started_at: st === "skipped" ? null : start,
          finished_at: st === "skipped" ? null : clock,
          next_at: null,
          output: st === "succeeded" ? demoOutput(s, opts.threads?.[0]?.thread) : null,
          error: failed ? (opts.error ?? "failed") : null,
          log: st === "skipped" ? [] : demoLog(s, start, failed ? opts.error : undefined),
        };
      });
      const run: AutomationRun = {
        id: rid,
        automation: a.id,
        name: a.name,
        status,
        event: { id: nid("aev_demo"), source: triggerSource(a.trigger), kind: eventKind(a), summary, at },
        steps,
        threads: opts.threads ?? [],
        cursor: this.host.next(),
        created_at: at,
        started_at: status === "skipped" ? null : at + 200,
        finished_at: status === "skipped" ? at : clock,
        reason: opts.skipped ?? (status === "failed" ? (opts.error ?? null) : null),
      };
      this.runs.set(rid, run);
      this.events.set(run.event.id, { id: run.event.id, source: run.event.source, kind: run.event.kind, key: `${run.event.source}:${run.event.id}`, at, data: demoEventData(a, summary), automation: a.id });
    };
    const [mainfix, triage, publish, run, approve] = list;
    const box = machines.find((m) => m.sandbox)?.id ?? mbp?.id ?? "";
    past(mainfix, t - 2 * DAY - 3 * HOUR, "succeeded", "acme/roll#88 Main is red: lander_retries_once flakes", { threads: [{ thread: "thr_demo_ci", machine: box, step: "fix" }] });
    past(mainfix, t - 5 * DAY - HOUR, "failed", "acme/roll#81 Main is red: e2e postgres restart", { failAt: 1, error: "the thread failed: cargo test exited 101" });
    for (let d = 0; d < 7; d++) {
      const at = startOfDay(t) - d * DAY + 9 * HOUR;
      if (at > t || [0, 6].includes(new Date(at).getDay())) continue;
      past(triage, at, d === 3 ? "failed" : "succeeded", "Weekdays at 09:00", d === 3 ? { failAt: 0, error: "no machine with prj_demo_roll is online" } : {});
    }
    past(publish, t - 50 * MIN, "succeeded", "Thread done: Add --json to roll code status", { threads: [] });
    past(publish, t - 1 * DAY - 2 * HOUR, "succeeded", "Thread done: Tidy the README's quick start");
    past(publish, t - 3 * DAY, "skipped", "Thread done: Spike: edge middleware", { skipped: "a filter didn't match: event.data.project is prj_demo_web" });
    past(run, t - 26 * HOUR, "succeeded", "Run by ada", { threads: [{ thread: "thr_demo_next", machine: mbp?.id ?? "", step: "start" }] });
    for (let i = 0; i < 9; i++) past(approve, t - i * 7 * HOUR - 20 * MIN, "succeeded", i % 2 ? "Request: cargo test -p roll-cli code" : "Request: pnpm test --filter dashboard");

    // One going right now: main broke a few minutes ago and an agent is on it.
    const live = this.putRun({
      id: nid("arn_demo"),
      automation: mainfix.id,
      name: mainfix.name,
      status: "waiting",
      event: { id: nid("aev_demo"), source: "code", kind: "work", summary: "acme/roll#93 Main is red: the flaky queue test", at: t - 4 * MIN },
      steps: mainfix.steps.map((s, i) => ({
        id: s.id,
        kind: s.kind,
        status: i === 0 ? "succeeded" : i === 1 ? "waiting" : "pending",
        attempt: i < 2 ? 1 : 0,
        started_at: i === 0 ? t - 4 * MIN : i === 1 ? t - 4 * MIN + 3000 : null,
        finished_at: i === 0 ? t - 4 * MIN + 2900 : null,
        next_at: null,
        output: i === 0 ? { thread: "thr_demo_flaky", machine: mbp?.id, project: "prj_demo_roll", branch: "roll/fix-the-flaky-queue-test" } : null,
        error: null,
        log: i === 0 ? demoLog(s, t - 4 * MIN) : i === 1 ? [{ at: t - 4 * MIN + 3000, level: "info", text: "Waiting for thr_demo_flaky to be done or fail (up to 1h 30m)" }] : [],
      })),
      threads: [{ thread: "thr_demo_flaky", machine: mbp?.id ?? "", step: "fix" }],
      cursor: 0,
      created_at: t - 4 * MIN,
      started_at: t - 4 * MIN + 100,
      finished_at: null,
      reason: null,
    });
    this.events.set(live.event.id, { id: live.event.id, source: "code", kind: "work", key: `code:${live.event.id}`, at: live.event.at, data: { repo: "acme/roll", number: 93, title: "Main is red: the flaky queue test", state: "open", labels: ["broken-main"] }, automation: mainfix.id });
    for (const a of list) {
      const last = [...this.runs.values()].filter((r) => r.automation === a.id).sort((x, y) => y.created_at - x.created_at)[0];
      if (last) this.automations.set(a.id, { ...this.automations.get(a.id)!, last_run: { id: last.id, status: last.status, at: last.created_at } });
    }
    // Notices from before the page opened (the ones a person missed).
    const ended = (a: Automation, status: RunState) => [...this.runs.values()].filter((r) => r.automation === a.id && r.status === status).sort((x, y) => y.created_at - x.created_at)[0];
    const fixed = ended(mainfix, "succeeded");
    const missed = ended(triage, "failed");
    if (missed) this.notices.push({ id: nid("ntc_demo"), title: "Morning triage didn't run", body: missed.reason ?? undefined, level: "error", link: null, automation: triage.id, run: missed.id, at: missed.finished_at ?? missed.created_at });
    if (fixed) this.notices.push({ id: nid("ntc_demo"), title: "Main is green again", body: "An agent fixed lander_retries_once flaking on acme/roll#88.", level: "success", link: null, automation: mainfix.id, run: fixed.id, at: fixed.finished_at ?? fixed.created_at });
    this.notices.sort((a, b) => b.at - a.at);

    // It finishes on its own a little later, so the run detail moves while you watch.
    this.host.after(24_000, () => this.finishWaiting(live.id, "done", "Fixed: the queue test waited on a timer; it now waits on the event."));
  }

  private finishWaiting(rid: string, status: "done" | "failed", summary?: string) {
    const r = this.runs.get(rid);
    if (!r || isRunFinal(r.status)) return;
    const w = r.steps.find((s) => s.status === "waiting");
    if (!w) return;
    const said = summary ?? (status === "done" ? "The agent finished; its work is in its worktree, ready to review." : "The agent stopped with an error.");
    this.patchStep(rid, w.id, { status: "succeeded", finished_at: Date.now(), output: { outcome: status, thread: r.threads.at(-1)?.thread ?? null, run: null, status: "idle", error: status === "failed" ? said : null, message: said } }, { status: "running" });
    this.log(rid, w.id, `The thread is ${status}`);
    this.host.after(600, () => void this.advance(rid));
  }

  // ── running ──

  private startRun(a: Automation, source: string, kind: string, data: unknown, summary: string): AutomationRun {
    const at = Date.now();
    const evId = nid("aev_demo");
    this.events.set(evId, { id: evId, source, kind, key: `${source}:${evId}`, at, data, automation: a.id });
    const run = this.putRun({
      id: nid("arn_demo"),
      automation: a.id,
      name: a.name,
      status: "queued",
      event: { id: evId, source, kind, summary, at },
      steps: a.steps.map((s) => ({ id: s.id, kind: s.kind, status: "pending", attempt: 0, started_at: null, finished_at: null, next_at: null, output: null, error: null, log: [] })),
      threads: [],
      cursor: 0,
      created_at: at,
      started_at: null,
      finished_at: null,
      reason: null,
    });
    this.automations.set(a.id, { ...a, last_run: { id: run.id, status: run.status, at } });
    this.host.after(500, () => {
      const r = this.runs.get(run.id);
      if (r && r.status === "queued") this.putRun({ ...r, status: "running", started_at: Date.now() });
      void this.advance(run.id);
    });
    return run;
  }

  /** Run the next pending step of a run. */
  private async advance(rid: string) {
    const r = this.runs.get(rid);
    if (!r || isRunFinal(r.status)) return;
    const a = this.automations.get(r.automation);
    const next = r.steps.find((s) => s.status === "pending" || s.status === "retrying");
    if (!next || !a) {
      const failed = r.steps.some((s) => s.status === "failed");
      this.putRun({ ...r, status: failed ? "failed" : "succeeded", finished_at: Date.now() });
      return;
    }
    const s = a.steps.find((x) => x.id === next.id)!;
    this.patchStep(rid, s.id, { status: "running", attempt: next.attempt + 1, started_at: Date.now() }, { status: "running" });
    const ctx = this.context(r, a);
    const cfg = render(s.config, ctx) as Record<string, unknown>;
    try {
      const out = await this.runStep(rid, s, cfg);
      if (out === "waiting") return;
      this.patchStep(rid, s.id, { status: "succeeded", finished_at: Date.now(), output: out });
      this.host.after(500, () => void this.advance(rid));
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      this.log(rid, s.id, msg, "error");
      this.patchStep(rid, s.id, { status: "failed", finished_at: Date.now(), error: msg });
      const cur = this.runs.get(rid)!;
      if (s.on_error === "continue") this.host.after(400, () => void this.advance(rid));
      else this.putRun({ ...cur, status: "failed", finished_at: Date.now(), reason: `${s.id}: ${msg}`, steps: cur.steps.map((x) => (x.status === "pending" ? { ...x, status: "skipped" } : x)) });
    }
  }

  private context(r: AutomationRun, a: Automation): Record<string, unknown> {
    const steps: Record<string, unknown> = {};
    for (const s of r.steps) if (s.output) steps[s.id] = s.output;
    return { event: { ...this.events.get(r.event.id), data: this.events.get(r.event.id)?.data }, steps, run: { id: r.id, threads: r.threads }, automation: { id: a.id, name: a.name }, owner: "demo" };
  }

  private async runStep(rid: string, s: Step, cfg: Record<string, unknown>): Promise<unknown> {
    const wait = (ms: number) => new Promise<void>((res) => this.host.after(ms, res));
    switch (s.kind) {
      case "agent.start": {
        const machines = this.host.machines();
        const projects = this.host.projects();
        const target = cfg.target as { type: string; machine?: string; size?: string } | null;
        const pref = cfg.project as { type: string; id?: string; repo?: string } | null;
        let project = pref?.type === "id" ? projects.find((p) => p.id === pref.id) : pref?.type === "repo" ? projects.find((p) => p.repo === pref.repo) : projects[0];
        let machine = target?.type === "machine" ? machines.find((m) => m.id === target.machine) : project ? machines.find((m) => m.id === project!.machine) : machines.find((m) => m.online);
        if (target?.type === "sandbox") {
          this.log(rid, s.id, `Waking a ${target.size ?? "standard"} sandbox`);
          machine = machines.find((m) => m.sandbox) ?? machine;
          await wait(900);
        }
        if (!machine) throw new Error("no machine is online");
        if (!project || project.machine !== machine.id) project = projects.find((p) => p.machine === machine!.id) ?? project;
        if (!project) throw new Error(`no project on ${machine.name}`);
        this.log(rid, s.id, `Starting ${cfg.model ?? cfg.provider ?? "an agent"} on ${machine.name} in ${project.name}${cfg.worktree ? " (worktree)" : ""}`);
        const reply = await this.host.command(machine.id, {
          type: "thread.create",
          project: project.id,
          provider: (cfg.provider as string) || "claude",
          ...(cfg.model ? { model: cfg.model as string } : {}),
          ...(cfg.effort ? { effort: cfg.effort as string } : {}),
          worktree: !!cfg.worktree,
          ...(cfg.title ? { title: cfg.title as string } : {}),
          prompt: (cfg.prompt as string) || "Look around and say what you see.",
          orchestrator: !!cfg.orchestrator,
        });
        if (reply.type !== "created") throw new Error(reply.type === "error" ? reply.message : "the machine didn't start it");
        const r = this.runs.get(rid)!;
        this.putRun({ ...r, threads: [...r.threads, { machine: machine.id, thread: reply.id, step: s.id }] });
        this.log(rid, s.id, `Thread ${reply.id} started`);
        return { thread: reply.id, machine: machine.id, project: project.id, branch: cfg.worktree ? `roll/${reply.id}` : null };
      }
      case "agent.wait": {
        const thread = (cfg.thread as string) || this.runs.get(rid)!.threads.at(-1)?.thread;
        if (!thread) throw new Error("no thread to wait for");
        this.patchStep(rid, s.id, { status: "waiting" }, { status: "waiting" });
        this.log(rid, s.id, `Waiting for ${thread}`);
        const started = Date.now();
        const poll = () => {
          const st = this.host.threadStatus(thread);
          const r = this.runs.get(rid);
          if (!r || isRunFinal(r.status)) return;
          if (st === "idle" || st === "error" || st === undefined || Date.now() - started > 90_000) this.finishWaiting(rid, st === "error" ? "failed" : "done");
          else this.host.after(1000, poll);
        };
        this.host.after(2500, poll);
        return "waiting";
      }
      case "notify": {
        await wait(300);
        const r = this.runs.get(rid)!;
        const notice: Notice = { id: nid("ntc_demo"), title: String(cfg.title ?? "Notice"), body: cfg.body ? String(cfg.body) : undefined, level: (cfg.level as "info") ?? "info", link: (cfg.link as string) ?? null, automation: r.automation, run: rid, at: Date.now() };
        this.notices.unshift(notice);
        this.host.emit({ type: "notice", notice });
        this.log(rid, s.id, "Notice sent");
        return { id: nid("ntc_") };
      }
      case "delay": {
        const sec = Math.min(Number(cfg.seconds) || 1, 4);
        this.log(rid, s.id, `Waiting ${cfg.seconds}s`);
        await wait(sec * 1000);
        return {};
      }
      case "webhook": {
        this.log(rid, s.id, `${cfg.method ?? "POST"} ${cfg.url}`);
        await wait(700);
        this.log(rid, s.id, "200 OK in 182 ms");
        return { status: 200, body: '{"ok":true}', json: { ok: true } };
      }
      case "agent.publish": {
        await wait(1100);
        this.log(rid, s.id, "Published acme/roll#94");
        if (cfg.land) this.log(rid, s.id, "Queued to land (position 1)");
        return { change: 94, url: "https://roll.sh/acme/roll/changes/94", queued: !!cfg.land };
      }
      default: {
        // An integration's step: as the demo shows it connected, it goes (one not connected says so).
        const known = CATALOG.steps.find((k) => k.kind === s.kind);
        if (known?.needs_setup) throw new Error(`connect ${known.integration ?? s.kind.split(".")[0]} first`);
        if (known?.integration) {
          await wait(900);
          this.log(rid, s.id, `${known.title}: done (demo)`);
          return { ok: true, id: nid(`${known.integration}_`) };
        }
        await wait(800);
        this.log(rid, s.id, `${BUILT_IN_STEPS[s.kind as keyof typeof BUILT_IN_STEPS]?.title ?? s.kind}: done`);
        return {};
      }
    }
  }

  // ── the API ──

  api(): AutomationsApi {
    const me = this;
    const spec = (s: AutomationSpec) => {
      if (!s.name.trim()) throw new ApiError("name: give it a name", 400, null);
      if (!s.steps.length) throw new ApiError("steps: at least one step", 400, null);
      return s;
    };
    const saved = (a: Automation, secret: string | null = null) => ({ automation: a, webhook_secret: secret });
    const hook = (id: string) => ({ url: `https://roll.sh/v1/code/agents/demo/automations/${id}/hook`, secret_hint: "" });
    return {
      async list() {
        me.seed();
        return [...me.automations.values()].sort((a, b) => a.created_at - b.created_at);
      },
      async get(_o, id) {
        me.seed();
        return me.must(id);
      },
      async create(_o, s) {
        me.seed();
        spec(s);
        const id = nid("aut_demo");
        const secret = s.trigger.type === "webhook" ? `rlh_${Math.random().toString(36).slice(2, 14)}${Math.random().toString(36).slice(2, 14)}` : null;
        const a = me.putAutomation({ ...s, id, revision: 1, cursor: 0, created_at: Date.now(), updated_at: Date.now(), created_by: "ada", secrets: [], webhook: secret ? { ...hook(id), secret_hint: secret.slice(-4) } : null, next_at: s.trigger.type === "schedule" ? nextWeekday9(Date.now()) : null, last_run: null });
        return saved(a, secret);
      },
      async update(_o, id, s, revision) {
        const a = me.must(id);
        if (revision !== undefined && revision !== a.revision) throw new ApiError("someone saved a newer version", 409, { current: a });
        spec(s);
        let secret: string | null = null;
        let webhook = a.webhook;
        if (s.trigger.type === "webhook" && !webhook) {
          secret = `rlh_${Math.random().toString(36).slice(2, 14)}${Math.random().toString(36).slice(2, 14)}`;
          webhook = { ...hook(id), secret_hint: secret.slice(-4) };
        } else if (s.trigger.type !== "webhook") webhook = null;
        return saved(me.putAutomation({ ...a, ...s, revision: a.revision + 1, webhook }), secret);
      },
      async remove(_o, id) {
        me.must(id);
        me.automations.delete(id);
        me.host.emit({ type: "automation_removed", cursor: me.host.next(), automation: id });
      },
      async enable(_o, id) {
        return me.putAutomation({ ...me.must(id), enabled: true });
      },
      async disable(_o, id) {
        return me.putAutomation({ ...me.must(id), enabled: false });
      },
      async run(_o, id, req = {}) {
        const a = me.must(id);
        return me.startRun(a, "manual", "run", { ...(req.data ?? {}), by: "ada", input: req.input ?? null }, "Run by ada");
      },
      async runsOf(_o, id, q = {}) {
        me.seed();
        return page([...me.runs.values()].filter((r) => r.automation === id), q);
      },
      async runs(_o, q = {}) {
        me.seed();
        return page([...me.runs.values()].filter((r) => !q.automation || r.automation === q.automation), q);
      },
      async notices(_o, q = {}) {
        me.seed();
        return me.notices.filter((n) => q.before === undefined || n.at < q.before).slice(0, q.limit ?? 50);
      },
      async runDetail(_o, rid) {
        me.seed();
        const run = me.runs.get(rid);
        if (!run) throw new ApiError("no such run", 404, null);
        const event = me.events.get(run.event.id) ?? { id: run.event.id, source: run.event.source, kind: run.event.kind, key: run.event.id, at: run.event.at, data: null, automation: run.automation };
        return { run, event };
      },
      async cancel(_o, rid) {
        const r = me.runs.get(rid);
        if (!r) throw new ApiError("no such run", 404, null);
        if (isRunFinal(r.status)) return r;
        return me.putRun({ ...r, status: "cancelled", finished_at: Date.now(), reason: "cancelled by ada", steps: r.steps.map((s) => (isStepOpen(s.status) ? { ...s, status: "cancelled", finished_at: s.started_at ? Date.now() : null } : s)) });
      },
      async retry(_o, rid) {
        const r = me.runs.get(rid);
        if (!r) throw new ApiError("no such run", 404, null);
        if (r.status !== "failed" && r.status !== "cancelled") throw new ApiError("only a failed or cancelled run goes again", 409, null);
        // As the server does: the failure stays in the step's log, and the run says when it was retried.
        const now = Date.now();
        const why = r.status === "cancelled" ? "Retried after the run was cancelled" : `Retried after the run failed${r.reason ? `: ${r.reason}` : ""}`;
        const again = (s: StepRun): StepRun => ({
          ...s,
          status: "pending",
          attempt: 0,
          error: null,
          finished_at: null,
          log: [...s.log, ...(s.error && s.log.at(-1)?.text !== s.error ? [{ at: s.finished_at ?? now, level: "error" as const, text: s.error }] : []), { at: now, level: "info" as const, text: why }],
        });
        const next = me.putRun({ ...r, status: "running", finished_at: null, reason: null, retried_at: now, steps: r.steps.map((s) => (s.status === "failed" || s.status === "cancelled" ? again(s) : s.status === "skipped" && s.started_at === null ? { ...s, status: "pending", attempt: 0, finished_at: null } : s)) });
        me.host.after(400, () => void me.advance(rid));
        return next;
      },
      async setSecret(_o, id, name) {
        const a = me.must(id);
        if (!a.secrets.includes(name)) me.putAutomation({ ...a, secrets: [...a.secrets, name].sort() });
      },
      async removeSecret(_o, id, name) {
        const a = me.must(id);
        me.putAutomation({ ...a, secrets: a.secrets.filter((s) => s !== name) });
      },
      async rotateWebhookSecret(_o, id) {
        const a = me.must(id);
        const secret = `rlh_${Math.random().toString(36).slice(2, 14)}${Math.random().toString(36).slice(2, 14)}`;
        return saved(me.putAutomation({ ...a, webhook: { ...hook(id), secret_hint: secret.slice(-4) } }), secret);
      },
      async catalog() {
        return CATALOG;
      },
      async test(_o, req): Promise<TestResult> {
        const a = req.automation;
        const ctx = { event: { source: req.event.source, kind: req.event.kind, id: "aev_test", data: req.event.data }, steps: {}, run: { id: "arn_test", threads: [] }, automation: { id: "aut_test", name: a.name }, owner: "demo" };
        const trigger = triggerSource(a.trigger) === req.event.source && (!("events" in a.trigger) || !a.trigger.events.length || (a.trigger.events as string[]).includes(req.event.kind) || a.trigger.type === "code");
        const filters = a.filters.map((f) => holds(f, ctx));
        const steps = a.steps.map((s) => {
          const known = CATALOG.steps.find((k) => k.kind === s.kind);
          if (!known) return { id: s.id, runs: false, config: s.config, error: `no step kind ${s.kind}` };
          const runs = s.when.every((f) => holds(f, ctx));
          return { id: s.id, runs, config: render(s.config, ctx), error: known.needs_setup ? `connect ${known.integration} first` : null };
        });
        return { matched: trigger && filters.every(Boolean), trigger, filters, steps };
      },
    };
  }
}

const isStepOpen = (s: StepRun["status"]) => s === "pending" || s === "running" || s === "waiting" || s === "retrying";

function page(runs: AutomationRun[], q: { before?: string; limit?: number; status?: RunState }) {
  let list = runs.sort((a, b) => b.created_at - a.created_at);
  if (q.status) list = list.filter((r) => r.status === q.status);
  if (q.before) {
    const i = list.findIndex((r) => r.id === q.before);
    list = i < 0 ? [] : list.slice(i + 1);
  }
  return list.slice(0, Math.min(q.limit ?? 50, 200));
}

function eventKind(a: Automation): string {
  const t = a.trigger;
  if (t.type === "manual") return "run";
  if (t.type === "schedule" || t.type === "interval") return "tick";
  if (t.type === "webhook") return "post";
  return t.events[0] ?? "event";
}

function demoEventData(a: Automation, summary: string): unknown {
  const t = a.trigger;
  if (t.type === "code") return { repo: t.repos[0] ?? "acme/roll", number: Number(/#(\d+)/.exec(summary)?.[1] ?? 1), title: summary.replace(/^.*?#\d+ /, ""), state: "open", labels: ["broken-main"] };
  // The server's shape (`agent_events` in triggers.rs): the thread's brief, then the run or the request.
  if (t.type === "agent") {
    const data = { machine: "mch_demo_ada", project: "prj_demo_roll", thread: { id: "thr_demo_json", title: summary.replace(/^[^:]+: /, ""), status: "idle", provider: "codex" } };
    if (t.events.includes("request.opened")) return { ...data, run: { id: "run_demo_json" }, request: { id: "req_demo_json", status: "pending", body: { type: "approval", action: "command", title: summary, command: summary.replace(/^Request: /, "") } } };
    return { ...data, run: { id: "run_demo_json", status: "completed", error: null } };
  }
  if (t.type === "manual") return { by: "ada", input: { prompt: "Pick an open work item and fix it." } };
  if (t.type === "webhook") return { body: { service: "api", log: "error: health check failed" }, headers: { "content-type": "application/json" } };
  return { at: Date.now(), scheduled: Date.now() };
}

function demoOutput(s: Step, thread?: string): unknown {
  switch (s.kind) {
    case "agent.start":
      return { thread: thread ?? "thr_demo_json", machine: "mch_demo_ada", project: "prj_demo_roll", sandbox: null };
    case "agent.wait":
      return { outcome: "done", thread: thread ?? "thr_demo_json", run: null, status: "idle", error: null, message: "Done: the change is small and has a test." };
    case "agent.publish":
      return { branch: "roll/fix", commit: "4f2c9e1", change: 88, url: "https://roll.sh/acme/roll/changes/88", landing: { status: "queued" } };
    case "agent.respond":
      return { answered: 1, left: 0 };
    case "notify":
      return { id: "ntc_demo" };
    default:
      return {};
  }
}

function demoLog(s: Step, at: number, error?: string) {
  const lines: { at: number; level: "info" | "warn" | "error"; text: string }[] = [];
  const add = (dt: number, text: string, level: "info" | "warn" | "error" = "info") => lines.push({ at: at + dt, level, text });
  switch (s.kind) {
    case "agent.start":
      add(0, "Resolving the target");
      add(700, "Starting claude (Opus 5.5) in a worktree");
      add(2200, "Thread started");
      break;
    case "agent.wait":
      add(0, "Waiting for the thread to be done or fail");
      break;
    case "agent.publish":
      add(0, "Publishing the worktree as a change");
      add(800, "Queued to land");
      break;
    case "agent.respond":
      add(0, "Matched rule 1: accept");
      break;
    default:
      add(0, "Started");
  }
  if (error) add(1500, error, "error");
  return lines;
}

function startOfDay(t: number) {
  const d = new Date(t);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

function nextWeekday9(t: number) {
  let d = startOfDay(t) + 9 * HOUR;
  while (d <= t || [0, 6].includes(new Date(d).getDay())) d += DAY;
  return d;
}

// ── a stand-in for the server's templates and filters ──

function lookup(ctx: unknown, path: string): unknown {
  let cur = ctx;
  for (const p of path.split(".")) {
    if (cur === null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[p];
  }
  return cur;
}

const show = (v: unknown) => (v === undefined || v === null ? "" : typeof v === "string" ? v : typeof v === "object" ? JSON.stringify(v) : String(v));

/** `{{ path | default:"x" | upper | truncate:N }}`, enough of `template.rs` for the demo. */
export function renderTemplate(s: string, ctx: unknown): string {
  return s.replace(/{{\s*([^}|]+?)\s*((?:\|[^}]*)?)}}/g, (_m, path: string, pipes: string) => {
    let v: unknown = lookup(ctx, path.trim());
    for (const raw of pipes.split("|").slice(1)) {
      const t = raw.trim();
      const colon = t.indexOf(":");
      const name = colon < 0 ? t : t.slice(0, colon);
      const a = colon < 0 ? undefined : t.slice(colon + 1).trim().replace(/^"([\s\S]*)"$/, "$1");
      if (name === "default" && (v === undefined || v === null || v === "")) v = a && /^[a-z_]+(\.[\w]+)+$/.test(a) ? lookup(ctx, a) : a;
      else if (name === "upper") v = show(v).toUpperCase();
      else if (name === "lower") v = show(v).toLowerCase();
      else if (name === "trim") v = show(v).trim();
      else if (name === "truncate") v = show(v).length > Number(a) ? `${show(v).slice(0, Number(a))}…` : v;
      else if (name === "json") v = JSON.stringify(v ?? null);
      else if (name === "lines") v = show(v).split("\n").slice(0, Number(a)).join("\n");
    }
    return show(v);
  });
}

function render(v: unknown, ctx: unknown): unknown {
  if (typeof v === "string") return renderTemplate(v, ctx);
  if (Array.isArray(v)) return v.map((x) => render(x, ctx));
  if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, render(x, ctx)]));
  return v;
}

function holds(f: Filter, ctx: unknown): boolean {
  const v = lookup(ctx, f.field);
  const want = f.value;
  let ok: boolean;
  switch (f.op) {
    case "equals":
      ok = String(v) === String(want);
      break;
    case "contains":
      ok = Array.isArray(v) ? v.map(String).includes(String(want)) : show(v).toLowerCase().includes(String(want).toLowerCase());
      break;
    case "any_of":
      ok = Array.isArray(want) && want.map(String).includes(String(v));
      break;
    case "has_label":
      ok = Array.isArray(v) && v.some((l) => (typeof l === "object" && l ? (l as { name?: string }).name : l) === want);
      break;
    case "glob": {
      const re = new RegExp(`^${String(want).replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*\*\/?/g, "(?:[^/]+/)*").replace(/(?<!\))\*/g, "[^/]*")}$`);
      ok = (Array.isArray(v) ? v : [v]).some((x) => re.test(show(x)));
      break;
    }
    case "exists":
      ok = v !== undefined && v !== null;
      break;
    case "gt":
      ok = Number(v) > Number(want);
      break;
    case "lt":
      ok = Number(v) < Number(want);
      break;
  }
  return f.not ? !ok : ok;
}
