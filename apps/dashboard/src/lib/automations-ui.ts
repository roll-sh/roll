/**
 * What the Automations view computes, without React: one-line summaries of triggers and steps, cron
 * in words, the catalog with the built-ins filled in, the variables a template can use at a step,
 * the `{{` under the caret, inline validation, a sample event for "Test", and run counts. The wire
 * types are `automations-api.ts`; docs/code/automations-ui.md is the design.
 */

import {
  AGENT_EVENTS,
  CODE_EVENTS,
  isRunFinal,
  STEP_KINDS,
  type AgentEvent,
  type Automation,
  type AutomationRun,
  type AutomationSpec,
  type Catalog,
  type EventInfo,
  type Filter,
  type FilterOp,
  type RunState,
  type Step,
  type StepInfo,
  type StepState,
  type TestEvent,
  type Trigger,
  type TriggerInfo,
} from "./automations-api";
import builtInCatalog from "./automation-catalog.json";

// ── the catalog, with the built-ins ──

/** A JSON Schema, as far as the forms read one. */
export interface Schema {
  type?: "object" | "string" | "integer" | "number" | "boolean" | "array";
  title?: string;
  description?: string;
  properties?: Record<string, Schema>;
  required?: string[];
  enum?: string[];
  items?: Schema;
  default?: unknown;
  minimum?: number;
  maximum?: number;
  /**
   * The widget beyond the type (the catalog's convention, agreed with code-control):
   * `template`, `template-long`, `roll-thread`, `roll-machine`, `roll-target`, `roll-project`,
   * `roll-model` (writes the sibling `provider` and `effort` too), `roll-secret`, `json`, `lookup`
   * (a picker: see `lookup`).
   */
  format?: string;
  /** Labels for `enum` values, in order. */
  enumTitles?: string[];
  additionalProperties?: Schema | boolean;
  /** `format: "lookup"`: where the picker's rows come from (code-control `lookup::Pick`). */
  lookup?: SchemaLookup;
  /** Where the field goes among its siblings (code-control's `props!`: the order it was written in). */
  "x-order"?: number;
}

/**
 * A lookup field's rows: the server's `source/kind` (`linear/states`), its parent from the sibling
 * field `parent` (used when it holds one value), and the field's own `extras` shown first.
 */
export interface SchemaLookup {
  source?: string;
  kind?: string;
  parent?: string;
  extras?: { id: string; label: string }[];
  extras_group?: string;
  /** New ones can be made from the search. */
  create?: boolean;
}

const tpl = (title: string, description?: string, long = false): Schema => ({ type: "string", title, description, format: long ? "template-long" : "template" });
const thread: Schema = { type: "string", title: "Thread", description: "Empty: the thread this run started last.", format: "roll-thread" };

/** The built-in steps' forms, for a catalog entry that brings no `properties` (and the demo's catalog). */
export const BUILT_IN_STEPS: Record<(typeof STEP_KINDS)[number], { title: string; description: string; output: string[]; config: Schema }> = {
  "agent.start": {
    title: "Start an agent",
    description: "A new thread on a machine, a sandbox, or wherever the project is.",
    output: ["machine", "project", "thread", "sandbox"],
    config: {
      type: "object",
      required: ["prompt"],
      properties: {
        prompt: tpl("Prompt", "What the agent should do.", true),
        title: tpl("Title", "Empty: the agent names it."),
        target: { type: "object", title: "Runs on", format: "roll-target" },
        project: { type: "object", title: "Project", format: "roll-project" },
        model: { type: "string", title: "Model", format: "roll-model" },
        provider: { type: "string" },
        effort: { type: "string" },
        runtime_mode: { type: "string", title: "Access", enum: ["full_access", "auto_edit", "ask"], enumTitles: ["Full access", "Auto-edit", "Ask first"] },
        interaction_mode: { type: "string", title: "Mode", enum: ["default", "plan"], enumTitles: ["Build", "Plan first"], default: "default" },
        worktree: { type: "boolean", title: "Own worktree", description: "A new branch and folder, so it never touches your checkout.", default: true },
        orchestrator: { type: "boolean", title: "Orchestrator", description: "It may plan and start more agents." },
      },
    },
  },
  "agent.wait": {
    title: "Wait for the agent",
    description: "Until it's done, fails, waits on you or is settled.",
    output: ["outcome", "thread", "run", "status", "error", "message"],
    config: {
      type: "object",
      properties: {
        thread,
        until: { type: "array", title: "Until it", items: { type: "string", enum: ["done", "failed", "waiting", "settled"], enumTitles: ["is done", "fails", "waits on you", "is settled"] }, default: ["done", "failed"] },
        timeout_s: { type: "integer", title: "Give up after (seconds)", minimum: 1 },
        on_timeout: { type: "string", title: "Then", enum: ["fail", "continue"], enumTitles: ["Fail the run", "Go on"] },
      },
    },
  },
  "agent.message": {
    title: "Message the agent",
    description: "Send it a follow-up, queued or steering the turn it's on.",
    output: ["thread", "run"],
    config: {
      type: "object",
      required: ["text"],
      properties: {
        thread,
        text: tpl("Message", undefined, true),
        mode: { type: "string", title: "While it works", enum: ["queue", "steer"], enumTitles: ["Queue it", "Steer now"] },
      },
    },
  },
  "agent.respond": {
    title: "Answer its requests",
    description: "Approve or decline what it asks, by rule.",
    output: ["answered", "left"],
    config: {
      type: "object",
      properties: {
        thread,
        rules: {
          type: "array",
          title: "Rules",
          description: "The first rule that fits decides.",
          items: {
            type: "object",
            properties: {
              action: { type: "string", title: "When it asks to", enum: ["command", "file_change", "permission", "tool", "plan"], enumTitles: ["run a command", "change files", "get a permission", "use a tool", "follow a plan"] },
              command: { type: "string", title: "Command matches", format: "template" },
              files: { type: "string", title: "Files match" },
              tool: { type: "string", title: "Tool" },
              decision: { type: "string", title: "Answer", enum: ["accept", "accept_for_session", "decline", "cancel"], enumTitles: ["Accept", "Accept for the session", "Decline", "Cancel"] },
            },
          },
        },
        otherwise: { type: "string", title: "Otherwise", enum: ["accept", "accept_for_session", "decline", "cancel"], enumTitles: ["Accept", "Accept for the session", "Decline", "Cancel"] },
        first_option: { type: "boolean", title: "Pick the first option of questions" },
        message: tpl("Then tell it"),
      },
    },
  },
  "agent.publish": {
    title: "Publish as a change",
    description: "Its work as a roll code change, queued to land if you like.",
    output: ["branch", "commit", "change", "url", "landing"],
    config: {
      type: "object",
      properties: {
        thread,
        message: tpl("Change title", "Empty: the thread's title."),
        land: { type: "boolean", title: "Queue it to land", description: "Through the merge queue: it lands once it's tested." },
      },
    },
  },
  "agent.script": {
    title: "Run a project script",
    description: "One of the project's scripts, in the thread's worktree or the project.",
    output: ["terminal", "machine"],
    config: {
      type: "object",
      required: ["script"],
      properties: {
        script: { type: "string", title: "Script", description: "Its id in the project's settings (setup, test, …)." },
        thread,
        project: { type: "object", title: "Project", format: "roll-project" },
        target: { type: "object", title: "Runs on", format: "roll-target" },
      },
    },
  },
  "agent.settle": {
    title: "Settle the thread",
    description: "Off the canvas into the settled shelf, archived, or back.",
    output: ["thread", "action"],
    config: { type: "object", properties: { thread, action: { type: "string", title: "Action", enum: ["settle", "archive", "unsettle"], enumTitles: ["Settle", "Archive", "Unsettle"] } } },
  },
  webhook: {
    title: "Call a webhook",
    description: "An HTTP request anywhere public, signed if you like.",
    output: ["status", "headers", "body"],
    config: {
      type: "object",
      required: ["url"],
      properties: {
        url: tpl("URL"),
        method: { type: "string", title: "Method", enum: ["POST", "PUT", "PATCH", "GET", "DELETE"] },
        headers: { type: "object", title: "Headers", description: "Values may use {{secrets.NAME}}.", additionalProperties: { type: "string", format: "template" } },
        body: { title: "Body", description: "Empty: the event as JSON.", format: "json" },
        secret: { type: "string", title: "Sign with", description: "X-Roll-Signature-256 with one of this automation's secrets.", format: "roll-secret" },
      },
    },
  },
  notify: {
    title: "Notify me",
    description: "A notice in the dashboard.",
    output: ["id"],
    config: {
      type: "object",
      required: ["title"],
      properties: {
        title: tpl("Title"),
        body: tpl("Body", undefined, true),
        level: { type: "string", title: "Level", enum: ["info", "success", "warning", "error"], enumTitles: ["Info", "Success", "Warning", "Error"] },
        link: tpl("Link"),
      },
    },
  },
  delay: {
    title: "Wait a while",
    description: "Pause the run before the next step.",
    output: [],
    config: { type: "object", required: ["seconds"], properties: { seconds: { type: "integer", title: "Seconds", minimum: 1, default: 60 } } },
  },
};

/** An event a trigger brings: its kind, title, the fields templates can read, and a sample (`null`: made from the fields). */
export const ev = (kind: string, title: string, fields: string[], sample: unknown = null): EventInfo => ({ kind, title, fields, sample });

/**
 * A built-in trigger's events as the server's registry has them (their fields and a sample of
 * their `event.data`, `automation-catalog.json`), titled for people.
 */
function eventsOf(source: string, titles: Record<string, string>): EventInfo[] {
  const t = (builtInCatalog as unknown as Catalog).triggers.find((x) => x.source === source);
  return (t?.events ?? []).map((e) => ev(e.kind, titles[e.kind] ?? e.kind, e.fields, e.sample));
}

/** The built-in triggers, with the fields their events bring (what `{{event.data.…}}` offers). */
export const BUILT_IN_TRIGGERS: TriggerInfo[] = [
  { source: "manual", title: "By hand", description: "A button here, `roll agent automation run`, or MCP.", events: eventsOf("manual", { run: "Run" }), config: null },
  { source: "schedule", title: "On a schedule", description: "A cron schedule in your time zone.", events: eventsOf("schedule", { tick: "Tick" }), config: null },
  { source: "interval", title: "Every so often", description: "Every few minutes or hours.", events: eventsOf("interval", { tick: "Tick" }), config: null },
  { source: "webhook", title: "Webhook", description: "A signed POST to its own URL, from anything.", events: eventsOf("webhook", { post: "Request" }), config: null },
  {
    source: "code",
    title: "roll code",
    description: "Landings, changes, checks, reviews and work items.",
    events: eventsOf("code", { landed: "Landed", change: "Change", check: "Check", review: "Review", queue: "Merge queue", work: "Work item", preview: "Preview", push: "Push", thread: "Discussion" }),
    config: null,
  },
  {
    source: "agent",
    title: "Agents",
    description: "A thread finishing, failing, waiting on you or asking.",
    events: eventsOf("agent", {
      "thread.created": "Thread started",
      "thread.done": "Thread done",
      "thread.failed": "Thread failed",
      "thread.waiting": "Waiting on you",
      "thread.settled": "Thread settled",
      "request.opened": "Request opened",
    }),
    config: null,
  },
];

const BUILT_IN_TRIGGER_TYPES = new Set(["manual", "schedule", "interval", "webhook", "code", "agent"]);

const hasProperties = (s: unknown): s is Schema => typeof s === "object" && s !== null && "properties" in s;

/**
 * The catalog the builder uses: the server's, with every built-in present and a built-in step's
 * form filled in when the server's entry brings none.
 */
export function withBuiltIns(c: Catalog | null): Catalog {
  const triggers = [...(c?.triggers ?? [])];
  for (const t of BUILT_IN_TRIGGERS) {
    const i = triggers.findIndex((x) => x.source === t.source);
    if (i < 0) triggers.push(t);
    else if (!triggers[i].events.length) triggers[i] = { ...triggers[i], events: t.events };
  }
  const order = (s: string) => {
    const i = BUILT_IN_TRIGGERS.findIndex((t) => t.source === s);
    return i < 0 ? BUILT_IN_TRIGGERS.length : i;
  };
  triggers.sort((a, b) => order(a.source) - order(b.source));
  const steps: StepInfo[] = (c?.steps ?? []).map((s) => {
    const b = BUILT_IN_STEPS[s.kind as keyof typeof BUILT_IN_STEPS];
    return b && !hasProperties(s.config) ? { ...s, config: b.config, output: s.output.length ? s.output : b.output } : s;
  });
  for (const kind of STEP_KINDS) {
    if (steps.some((s) => s.kind === kind)) continue;
    const b = BUILT_IN_STEPS[kind];
    steps.push({ kind, title: b.title, description: b.description, config: b.config, output: b.output, integration: null });
  }
  return { triggers, steps };
}

export const triggerSource = (t: Trigger) => (t.type === "integration" ? t.source : t.type);

export function triggerInfo(c: Catalog, t: Trigger): TriggerInfo | undefined {
  return c.triggers.find((x) => x.source === triggerSource(t));
}

export function stepInfo(c: Catalog, kind: string): StepInfo | undefined {
  return c.steps.find((s) => s.kind === kind);
}

export const schemaOf = (info: StepInfo | undefined): Schema => (hasProperties(info?.config) ? (info.config as Schema) : { type: "object", properties: {} });

export const isBuiltInTrigger = (source: string) => BUILT_IN_TRIGGER_TYPES.has(source);

// ── words ──

const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const pad = (n: number) => String(n).padStart(2, "0");

/** The `@` names croner takes, as the five fields they stand for. */
const CRON_MACROS: Record<string, string> = {
  "@yearly": "0 0 1 1 *",
  "@annually": "0 0 1 1 *",
  "@monthly": "0 0 1 * *",
  "@weekly": "0 0 * * 0",
  "@daily": "0 0 * * *",
  "@midnight": "0 0 * * *",
  "@hourly": "0 * * * *",
};

/** A cron expression in words when it's a common shape ("Weekdays at 09:00"), else `null`. */
export function cronText(cron: string): string | null {
  const name = cron.trim().toLowerCase();
  if (name === "@yearly" || name === "@annually") return "Yearly on 1 January at 00:00";
  if (CRON_MACROS[name]) return cronText(CRON_MACROS[name]);
  const f = cron.trim().split(/\s+/);
  // Six fields lead with seconds: only a whole minute (0) reads as the five after it.
  if (f.length === 6 && f[0] !== "0") {
    const every = /^\*\/(\d+)$/.exec(f[0])?.[1];
    return every && f.slice(1).every((x) => x === "*") ? `Every ${every} seconds` : f[0] === "*" && f.slice(1).every((x) => x === "*") ? "Every second" : null;
  }
  const p = f.length === 6 ? f.slice(1) : f;
  if (p.length !== 5) return null;
  const [min, hour, dom, mon, dow] = p;
  const num = (s: string) => (/^\d+$/.test(s) ? Number(s) : null);
  const step = (s: string) => /^\*\/(\d+)$/.exec(s)?.[1];
  if (dom !== "*" || mon !== "*") {
    if (num(min) !== null && num(hour) !== null && num(dom) !== null && mon === "*" && dow === "*") return `Monthly on day ${dom} at ${pad(num(hour)!)}:${pad(num(min)!)}`;
    return null;
  }
  if (step(min) && hour === "*" && dow === "*") return `Every ${step(min)} minutes`;
  if (min === "*" && hour === "*" && dow === "*") return "Every minute";
  if (num(min) !== null && hour === "*" && dow === "*") return num(min) === 0 ? "Every hour" : `Every hour at :${pad(num(min)!)}`;
  if (num(min) !== null && step(hour) && dow === "*") return `Every ${step(hour)} hours`;
  const m = num(min);
  const h = num(hour);
  if (m === null || h === null) return null;
  const at = `${pad(h)}:${pad(m)}`;
  if (dow === "*") return `Daily at ${at}`;
  if (dow === "1-5" || dow === "MON-FRI") return `Weekdays at ${at}`;
  if (dow === "0,6" || dow === "6,0" || dow === "SAT,SUN") return `Weekends at ${at}`;
  if (num(dow) !== null && num(dow)! <= 7) return `${DAYS[num(dow)! % 7]}s at ${at}`;
  return null;
}

/**
 * Is it a cron expression the server takes (5 fields, 6 with seconds first, or an `@` name like
 * `@daily`), and does some date match it? Only the shape and the plain impossibles (the 31st of
 * February): the server's croner says the rest.
 */
export function cronShape(cron: string): string | null {
  const t = cron.trim();
  if (t.startsWith("@")) return CRON_MACROS[t.toLowerCase()] ? null : "Use @hourly, @daily, @weekly, @monthly or @yearly, or 5 fields.";
  const f = t.split(/\s+/).filter(Boolean);
  if (f.length !== 5 && f.length !== 6) return `A cron expression has 5 fields (minute hour day month weekday), or 6 with seconds first: this has ${f.length}.`;
  if (!f.every((x) => /^[\d*/,\-A-Za-z?LW#]+$/.test(x))) return "Each field is numbers, *, ranges (1-5), lists (1,3) or steps (*/15).";
  // Each value in its field's range (the server's croner refuses the rest); six fields lead with seconds.
  const ranges = (f.length === 6 ? [CRON_SECONDS] : []).concat(CRON_FIELDS);
  for (const [i, x] of f.entries()) {
    const [name, lo, hi] = ranges[i];
    for (const m of x.matchAll(/(?:^|[,-])(\d+)/g)) {
      const n = Number(m[1]);
      if (n < lo || n > hi) return `The ${name} is ${lo} to ${hi}: ${n} isn't one.`;
    }
  }
  return neverRuns(f.length === 6 ? f.slice(1) : f);
}

const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const MONTH_DAYS = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/** Days of the month that no month it names has, and no weekday to fall back on (cron ORs the two). */
function neverRuns([, , dom, mon, dow]: string[]): string | null {
  const list = (s: string) => (/^\d+(,\d+)*$/.test(s) ? s.split(",").map(Number) : null);
  const days = list(dom);
  const months = list(mon);
  if (!days || !months || (dow !== "*" && dow !== "?")) return null;
  if (months.some((m) => days.some((d) => d <= MONTH_DAYS[m - 1]))) return null;
  return `It would never run: ${months.map((m) => MONTHS[m - 1]).join(" or ")} ${months.length === 1 ? "has" : "have"} no day ${Math.min(...days)}.`;
}

const CRON_SECONDS: [string, number, number] = ["second", 0, 59];
const CRON_FIELDS: [string, number, number][] = [
  ["minute", 0, 59],
  ["hour", 0, 23],
  ["day of the month", 1, 31],
  ["month", 1, 12],
  ["weekday", 0, 7],
];

export function everyText(s: number): string {
  if (s % 86_400 === 0) return s === 86_400 ? "Every day" : `Every ${s / 86_400} days`;
  if (s % 3600 === 0) return s === 3600 ? "Every hour" : `Every ${s / 3600} hours`;
  if (s % 60 === 0) return s === 60 ? "Every minute" : `Every ${s / 60} minutes`;
  return `Every ${s} s`;
}

const agentEventText: Record<AgentEvent, string> = {
  "thread.created": "a thread starts",
  "thread.done": "a thread finishes",
  "thread.failed": "a thread fails",
  "thread.waiting": "a thread waits on you",
  "thread.settled": "a thread is settled",
  "request.opened": "an agent asks",
};

const codeEventText: Record<string, string> = {
  landed: "a change lands",
  change: "a change opens or moves",
  check: "a check reports",
  review: "a review comes in",
  queue: "the merge queue moves",
  work: "a work item changes",
  preview: "a preview is ready",
  push: "someone pushes",
  thread: "a discussion moves",
};

const orList = (xs: string[]) => (xs.length <= 1 ? (xs[0] ?? "") : `${xs.slice(0, -1).join(", ")} or ${xs.at(-1)}`);

/** The trigger in one line, as the list shows it. */
export function triggerSummary(t: Trigger, c?: Catalog): string {
  switch (t.type) {
    case "manual":
      return "When you run it";
    case "schedule": {
      const words = cronText(t.cron) ?? t.cron;
      return t.timezone && t.timezone !== "UTC" ? `${words} · ${t.timezone.split("/").at(-1)?.replace(/_/g, " ")}` : `${words} UTC`;
    }
    case "interval":
      return everyText(t.every_s);
    case "webhook":
      return "When its webhook is called";
    case "code": {
      const evs = t.events.length ? t.events.map((e) => codeEventText[e] ?? e) : ["anything happens"];
      const where = t.repos.length === 1 ? ` in ${t.repos[0]}` : t.repos.length > 1 ? ` in ${t.repos.length} repos` : "";
      return `When ${orList(evs)}${where}`;
    }
    case "agent":
      return t.events.length ? `When ${orList(t.events.map((e) => agentEventText[e]))}` : "On any agent event";
    case "integration": {
      const info = c?.triggers.find((x) => x.source === t.source);
      const evs = t.events.map((e) => info?.events.find((x) => x.kind === e)?.title.toLowerCase() ?? e);
      return `${info?.title ?? t.source}${evs.length ? `: ${orList(evs)}` : ""}`;
    }
  }
}

export const filterOps: { op: FilterOp; label: string; needs: "value" | "list" | "none" | "number" }[] = [
  { op: "equals", label: "is", needs: "value" },
  { op: "contains", label: "contains", needs: "value" },
  { op: "any_of", label: "is any of", needs: "list" },
  { op: "has_label", label: "has label", needs: "value" },
  { op: "glob", label: "matches path", needs: "value" },
  { op: "exists", label: "is set", needs: "none" },
  { op: "gt", label: "is more than", needs: "number" },
  { op: "lt", label: "is less than", needs: "number" },
];

export function filterText(f: Filter): string {
  const op = filterOps.find((o) => o.op === f.op);
  const field = f.field.replace(/^event\.data\./, "");
  const v = Array.isArray(f.value) ? f.value.join(", ") : f.value === null || f.value === undefined ? "" : String(f.value);
  const label = f.not ? (f.op === "exists" ? "is not set" : `not ${op?.label ?? f.op}`) : (op?.label ?? f.op);
  return op?.needs === "none" ? `${field} ${label}` : `${field} ${label} ${v}`;
}

// ── templates ──

/** A variable a template can use at a step: `path` goes between `{{ }}`. */
export interface Variable {
  path: string;
  /** Where it comes from: "Event", "Run", a step's name. */
  group: string;
  hint?: string;
}

/**
 * What a template at step `at` (`-1`: the filters and the trigger) can read: the trigger's event
 * fields, the run and automation, and the outputs of the steps before it.
 */
export function variablesAt(spec: Pick<AutomationSpec, "trigger" | "steps">, c: Catalog, at: number): Variable[] {
  const out: Variable[] = [];
  const info = triggerInfo(c, spec.trigger);
  const wanted = spec.trigger.type === "code" || spec.trigger.type === "agent" || spec.trigger.type === "integration" ? spec.trigger.events : [];
  const seen = new Set<string>();
  for (const e of info?.events ?? []) {
    if (wanted.length && !wanted.includes(e.kind as never)) continue;
    for (const f of e.fields) {
      const path = `event.data.${f}`;
      if (seen.has(path)) continue;
      seen.add(path);
      out.push({ path, group: "Event", hint: wanted.length > 1 || !wanted.length ? e.title : undefined });
    }
  }
  if (spec.trigger.type === "code" && !seen.has("event.data.repo")) out.push({ path: "event.data.repo", group: "Event" });
  out.push({ path: "event.kind", group: "Event" }, { path: "event.source", group: "Event" }, { path: "event.id", group: "Event" });
  out.push({ path: "run.id", group: "Run" }, { path: "run.threads", group: "Run" }, { path: "automation.name", group: "Run" }, { path: "automation.id", group: "Run" }, { path: "owner", group: "Run" });
  spec.steps.slice(0, Math.max(0, at)).forEach((s, i) => {
    const outs = stepInfo(c, s.kind)?.output ?? [];
    for (const o of outs) out.push({ path: `steps.${s.id}.${o}`, group: s.name || `${i + 1}. ${stepInfo(c, s.kind)?.title ?? s.kind}` });
  });
  return out;
}

/**
 * The open `{{` the caret is in: where it starts and what's typed after it, so the picker can
 * filter and replace it. `null` when the caret isn't inside one.
 */
export function openTemplate(text: string, caret: number): { start: number; query: string } | null {
  const before = text.slice(0, caret);
  const open = before.lastIndexOf("{{");
  if (open < 0 || before.lastIndexOf("}}") > open) return null;
  const query = before.slice(open + 2);
  if (/[\n{}|]/.test(query)) return null;
  return { start: open, query: query.trim() };
}

/** Put `{{ path }}` in place of the open `{{…` that starts at `start` and ends at the caret. */
export function insertVariable(text: string, start: number, caret: number, path: string): { text: string; caret: number } {
  const rest = text.slice(caret);
  // An auto-closed `}}` right after the caret is the one this closes.
  const closing = /^\s*}}/.exec(rest);
  const tail = closing ? rest.slice(closing[0].length) : rest;
  const token = `{{ ${path} }}`;
  return { text: text.slice(0, start) + token + tail, caret: start + token.length };
}

/** Variables for the picker, the best first: prefix matches, then anywhere in the path. */
export function matchVariables(vars: readonly Variable[], query: string): Variable[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...vars];
  const scored = vars
    .map((v) => {
      const p = v.path.toLowerCase();
      const short = p.replace(/^event\.data\./, "");
      const score = p.startsWith(q) || short.startsWith(q) ? 0 : p.includes(q) ? 1 : -1;
      return { v, score };
    })
    .filter((x) => x.score >= 0);
  return scored.sort((a, b) => a.score - b.score).map((x) => x.v);
}

/** The variables a template uses (`{{ steps.start.thread | default:"x" }}` → `steps.start.thread`). */
export function templateRefs(text: string): string[] {
  const out: string[] = [];
  for (const m of text.matchAll(/{{\s*([^}|]+?)\s*(?:\|[^}]*)?}}/g)) out.push(m[1]);
  return out;
}

/** Split a template into text and `{{…}}` parts, for highlighting. */
export function templateParts(text: string): { text: string; token: boolean }[] {
  const out: { text: string; token: boolean }[] = [];
  let i = 0;
  for (const m of text.matchAll(/{{[^{}]*}}/g)) {
    if (m.index! > i) out.push({ text: text.slice(i, m.index), token: false });
    out.push({ text: m[0], token: true });
    i = m.index! + m[0].length;
  }
  if (i < text.length) out.push({ text: text.slice(i), token: false });
  return out;
}

// ── validation ──

/** Problems by where they are: `name`, `trigger`, `filters.0`, `steps.1`, `steps.1.config.prompt`. */
export type Problems = Record<string, string>;

const STEP_ID = /^[a-z0-9_]{1,32}$/;

function eachString(v: unknown, at: string, fn: (s: string, at: string) => void) {
  if (typeof v === "string") fn(v, at);
  else if (Array.isArray(v)) v.forEach((x, i) => eachString(x, `${at}.${i}`, fn));
  else if (v && typeof v === "object") for (const [k, x] of Object.entries(v)) eachString(x, `${at}.${k}`, fn);
}

/**
 * What's wrong with a spec before the server says so, for the builder's inline messages. `taken`:
 * the owner's other automations' names (two with one name can't be told apart in a list, a run or
 * a notice).
 */
export function validate(spec: AutomationSpec, c: Catalog, taken: readonly string[] = []): Problems {
  const p: Problems = {};
  const name = spec.name.trim();
  if (!name) p.name = "Give it a name.";
  else if (taken.some((t) => t.trim().toLowerCase() === name.toLowerCase())) p.name = `Another automation is called ${name}.`;
  const t = spec.trigger;
  if (t.type === "schedule") {
    const bad = cronShape(t.cron);
    if (bad) p["trigger.cron"] = bad;
    if (!t.timezone.trim()) p["trigger.timezone"] = "Pick a time zone.";
  } else if (t.type === "interval" && !(t.every_s >= 60)) p["trigger.every_s"] = "At least one minute.";
  else if (t.type === "agent" && !t.events.length) p.trigger = "Pick at least one agent event.";
  else if (t.type === "integration") {
    const info = triggerInfo(c, t);
    if (!info) p.trigger = `No integration ${t.source} on this server.`;
    else if (info.needs_setup) p.trigger = `Connect ${info.title} first.`;
  }
  spec.filters.forEach((f, i) => {
    if (!f.field.trim()) p[`filters.${i}`] = "Pick a field.";
    else if (f.op !== "exists" && (f.value === "" || f.value === null || (Array.isArray(f.value) && !f.value.length))) p[`filters.${i}`] = "Say what it should be.";
    else if ((f.op === "gt" || f.op === "lt") && !Number.isFinite(Number(f.value))) p[`filters.${i}`] = "A number.";
  });
  if (!spec.steps.length) p.steps = "Add a step: what should happen.";
  const ids = new Set<string>();
  spec.steps.forEach((s, i) => {
    const at = `steps.${i}`;
    if (!STEP_ID.test(s.id)) p[`${at}.id`] = "Lowercase letters, digits and _, up to 32.";
    else if (ids.has(s.id)) p[`${at}.id`] = `Another step is called ${s.id}.`;
    ids.add(s.id);
    const info = stepInfo(c, s.kind);
    if (!info) {
      p[at] = `No step ${s.kind} on this server.`;
      return;
    }
    if (info.needs_setup) p[at] = `Connect ${info.integration ?? "the integration"} first.`;
    const schema = schemaOf(info);
    const cfg = (s.config ?? {}) as Record<string, unknown>;
    for (const r of schema.required ?? []) {
      const v = cfg[r];
      if (v === undefined || v === null || (typeof v === "string" && !v.trim())) p[`${at}.config.${r}`] = `${schema.properties?.[r]?.title ?? r} is needed.`;
    }
    const earlier = new Set(spec.steps.slice(0, i).map((x) => x.id));
    const later = new Set(spec.steps.slice(i).map((x) => x.id));
    eachString(s.config, `${at}.config`, (str, where) => {
      if (p[where]) return;
      const opens = str.split("{{").length - 1;
      const closes = str.split("}}").length - 1;
      if (opens > closes) return void (p[where] = "A {{ isn't closed.");
      for (const ref of templateRefs(str)) {
        const m = /^steps\.([a-z0-9_]+)/.exec(ref);
        if (m && !earlier.has(m[1])) p[where] = later.has(m[1]) ? `${m[1]} runs after this step.` : `No step called ${m[1]}.`;
        if (ref.startsWith("secrets.") && !(s.kind === "webhook" && where.startsWith(`${at}.config.headers.`))) p[where] = "Secrets only go in a webhook's headers.";
      }
    });
  });
  if (spec.rate_limit && !(spec.rate_limit.max >= 1)) p.rate_limit = "At least one run.";
  return p;
}

/** The problems in a part of the spec (`steps.1` gets `steps.1` and `steps.1.config.prompt`). */
export const problemsIn = (p: Problems, prefix: string) => Object.entries(p).filter(([k]) => k === prefix || k.startsWith(`${prefix}.`));

// ── new things ──

/** A step id for `kind` none of `taken` has: `start`, `start_2`, … */
export function newStepId(kind: string, taken: Iterable<string>): string {
  const base =
    (
      {
        "agent.start": "start",
        "agent.wait": "wait",
        "agent.message": "message",
        "agent.respond": "answer",
        "agent.publish": "publish",
        "agent.script": "script",
        "agent.settle": "settle",
      } as Record<string, string>
    )[kind] ??
    kind
      .split(".")
      .at(-1)!
      .replace(/[^a-z0-9_]/g, "_")
      .slice(0, 28);
  const used = new Set(taken);
  if (!used.has(base)) return base;
  for (let i = 2; ; i++) if (!used.has(`${base}_${i}`)) return `${base}_${i}`;
}

/**
 * A new step's config: the schema's defaults, and an empty value for a required string or list
 * (its field asks for it). Every other field is left out: an unset field is absent, never `null`
 * (the server drops `null` fields too, docs/code/automations.md, "Steps").
 */
export function defaultConfig(schema: Schema): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, s] of Object.entries(schema.properties ?? {})) {
    if (s.default !== undefined) out[k] = s.default;
    else if (!schema.required?.includes(k)) continue;
    else if (s.type === "array") out[k] = [];
    else if (s.type === "string") out[k] = "";
  }
  return out;
}

/** `config` without its unset fields (`null`, `undefined`): what a form hands back after an edit. */
export function withoutUnset(config: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(config).filter(([, v]) => v != null));
}

export function newStep(kind: string, c: Catalog, taken: Iterable<string>): Step {
  return { id: newStepId(kind, taken), kind, config: defaultConfig(schemaOf(stepInfo(c, kind))), when: [], retry: null, timeout_s: null };
}

export function defaultTrigger(source: string, c: Catalog): Trigger {
  switch (source) {
    case "manual":
      return { type: "manual" };
    case "schedule":
      return { type: "schedule", cron: "0 9 * * 1-5", timezone: localZone() };
    case "interval":
      return { type: "interval", every_s: 3600 };
    case "webhook":
      return { type: "webhook" };
    case "code":
      return { type: "code", events: ["landed"], repos: [] };
    case "agent":
      return { type: "agent", events: ["thread.done"] };
    default: {
      const info = c.triggers.find((t) => t.source === source);
      return { type: "integration", source, events: info?.events[0] ? [info.events[0].kind] : [], config: {} };
    }
  }
}

export function localZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

export function blankSpec(): AutomationSpec {
  return { name: "", enabled: true, trigger: { type: "manual" }, filters: [], steps: [], concurrency: "parallel", rate_limit: null, run_as: null };
}

/** The editable part of an automation (what `PUT` replaces). */
export function specOf(a: Automation): AutomationSpec {
  const { name, description, enabled, trigger, filters, steps, defaults, concurrency, rate_limit, run_as, allow_agents } = a;
  return { name, description, enabled, trigger, filters, steps, defaults, concurrency, rate_limit, run_as, allow_agents };
}

export const sameSpec = (a: AutomationSpec, b: AutomationSpec) => JSON.stringify(normal(a)) === JSON.stringify(normal(b));
const normal = (s: AutomationSpec) => ({ ...s, description: s.description || undefined, allow_agents: s.allow_agents || undefined, defaults: s.defaults ?? undefined });

// ── testing ──

function setPath(o: Record<string, unknown>, path: string, v: unknown) {
  const parts = path.split(".");
  let cur = o;
  for (const p of parts.slice(0, -1)) cur = (cur[p] ??= {}) as Record<string, unknown>;
  cur[parts.at(-1)!] ??= v;
}

const SAMPLES: Record<string, unknown> = {
  repo: "acme/app",
  title: "Fix the flaky login test",
  number: 42,
  labels: ["bug"],
  thread: "thr_sample",
  machine: "mch_sample",
  project: "prj_sample",
  url: "https://roll.sh/acme/app/changes/42",
  author: "ada",
  by: "you",
  input: { prompt: "Look at the open work items" },
  at: 0,
  status: "failed",
  state: "open",
};

/**
 * An event to try the automation with: the trigger's first event kind, its fields filled with
 * believable values (or the server's own sample), then made to meet the automation's conditions
 * where a condition says what it wants, so the first test shows the run going.
 */
export function sampleEvent(t: Trigger, c: Catalog, now = Date.now(), filters: readonly Filter[] = []): TestEvent {
  const info = triggerInfo(c, t);
  const kinds = t.type === "code" || t.type === "agent" || t.type === "integration" ? t.events : [];
  const ev = info?.events.find((e) => !kinds.length || kinds.includes(e.kind as never)) ?? info?.events[0];
  const kind = ev?.kind ?? (t.type === "manual" ? "run" : "event");
  // The server's own sample, when its catalog has one (`EventInfo.sample`).
  const given = (ev as { sample?: unknown } | undefined)?.sample;
  const data: Record<string, unknown> = given && typeof given === "object" ? (structuredClone(given) as Record<string, unknown>) : {};
  if (!given) {
    for (const f of ev?.fields ?? []) {
      const leaf = f.split(".").at(-1)!;
      setPath(data, f, leaf === "at" || leaf === "scheduled" ? now : (SAMPLES[leaf] ?? `sample ${leaf}`));
    }
  }
  if (t.type === "code") data.repo = t.repos[0] ?? data.repo ?? "acme/app";
  for (const f of filters) {
    if (f.not || !f.field.startsWith("event.data.")) continue;
    const path = f.field.slice(11).split(".");
    const parent = path.slice(0, -1).reduce<Record<string, unknown>>((o, p) => (o[p] = typeof o[p] === "object" && o[p] !== null ? o[p] : {}) as Record<string, unknown>, data);
    const leaf = path.at(-1)!;
    if (f.op === "equals" || f.op === "contains") parent[leaf] = f.value;
    else if (f.op === "any_of" && Array.isArray(f.value) && f.value.length) parent[leaf] = f.value[0];
    else if (f.op === "has_label") parent[leaf] = [...(Array.isArray(parent[leaf]) ? (parent[leaf] as unknown[]) : []).filter((l) => l !== f.value), f.value];
    else if (f.op === "exists") parent[leaf] ??= `sample ${leaf}`;
    else if (f.op === "gt") parent[leaf] = Number(f.value) + 1;
    else if (f.op === "lt") parent[leaf] = Number(f.value) - 1;
  }
  return { source: triggerSource(t), kind, data };
}

// ── runs ──

export const DAY = 86_400_000;

/** Runs of each automation in the last 7 days, per day (oldest first), and how many failed. */
export function weekOf(runs: Iterable<AutomationRun>, automation: string, now = Date.now()): { days: { ok: number; failed: number }[]; total: number; failed: number } {
  const days = Array.from({ length: 7 }, () => ({ ok: 0, failed: 0 }));
  const start = startOfDay(now) - 6 * DAY;
  let total = 0;
  let failed = 0;
  for (const r of runs) {
    if (r.automation !== automation || r.created_at < start || r.status === "skipped") continue;
    const d = Math.min(6, Math.floor((r.created_at - start) / DAY));
    total++;
    if (r.status === "failed") {
      failed++;
      days[d].failed++;
    } else days[d].ok++;
  }
  return { days, total, failed };
}

function startOfDay(t: number) {
  const d = new Date(t);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

export type Tone = "accent" | "ok" | "danger" | "muted";

export const runTone: Record<RunState, Tone> = { queued: "muted", running: "accent", waiting: "accent", succeeded: "ok", failed: "danger", cancelled: "muted", skipped: "muted" };
export const runLabel: Record<RunState, string> = { queued: "Queued", running: "Running", waiting: "Waiting", succeeded: "Succeeded", failed: "Failed", cancelled: "Cancelled", skipped: "Skipped" };
export const stepLabel: Record<StepState, string> = {
  pending: "Pending",
  running: "Running",
  waiting: "Waiting",
  retrying: "Retrying",
  succeeded: "Done",
  failed: "Failed",
  skipped: "Skipped",
  cancelled: "Cancelled",
};

/** How long a run or step took (or has been going), `null` before it started. */
export function took(start: number | null, end: number | null, now = Date.now()): number | null {
  return start === null ? null : Math.max(0, (end ?? now) - start);
}

/** Where a run's time counts from: its last retry, else its start (a retried run sat failed in between). */
export const runStart = (r: AutomationRun): number | null => r.retried_at ?? r.started_at;

/**
 * A step's log line's time: "+1.2s" from the step's start, or, for a line from before it (an
 * attempt before a retry), the time of day.
 */
export function logTime(at: number, stepStart: number | null): string {
  if (stepStart !== null && at < stepStart) return new Date(at).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  return `+${((at - (stepStart ?? at)) / 1000).toFixed(1)}s`;
}

export function duration(ms: number): string {
  if (ms < 1000) return `${ms} ms`;
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${pad(s % 60)}s`;
  return `${Math.floor(m / 60)}h ${pad(m % 60)}m`;
}

export const canRetry = (r: AutomationRun) => r.status === "failed" || r.status === "cancelled";

/**
 * An automation's latest run as it is now: the newest of the runs the stream keeps current, else
 * the list's `last_run` (only as fresh as the last list).
 */
export function lastRunOf(a: Automation, runs: Iterable<AutomationRun>): { id: string; status: RunState; at: number } | null {
  let best: AutomationRun | null = null;
  for (const r of runs) if (r.automation === a.id && (!best || r.created_at > best.created_at)) best = r;
  if (best && (!a.last_run || best.created_at >= a.last_run.at)) return { id: best.id, status: best.status, at: best.finished_at ?? best.started_at ?? best.created_at };
  return a.last_run;
}
export const canCancel = (r: AutomationRun) => !isRunFinal(r.status);

export { AGENT_EVENTS, CODE_EVENTS };
