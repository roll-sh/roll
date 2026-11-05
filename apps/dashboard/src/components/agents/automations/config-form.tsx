"use client";

/**
 * A step's (or an integration trigger's) config form, made from its JSON Schema in the catalog
 * (`Schema` in automations-ui.ts): strings are template fields, enums T3's selects or segmented
 * controls, booleans switches, lists chips or rows; `format` asks for roll's own pickers (where it
 * runs, the project, the model, a secret). Problems show under the field they're about.
 */

import { Minus, Plus } from "lucide-react";
import { createContext, useContext, useMemo, type ReactNode } from "react";
import type { ModelOptions, ProviderInfo } from "@/lib/agents-api";
import type { ProjectRef, Target } from "@/lib/automations-api";
import type { Problems, Schema, Variable } from "@/lib/automations-ui";
import { LookupPicker } from "../lookup-picker";
import { ModelPicker } from "../model-picker";
import { field as fieldClass } from "../settings-ui";
import { cn, Segmented, Switch } from "../ui";
import { Picker, type PickerOption } from "../ui/picker";
import { useAgents } from "../use-agents";
import { NumberField, SizedSelect } from "./shared";
import { TemplateInput } from "./template-input";

export interface FormContext {
  variables: readonly Variable[];
  problems: Problems;
  /** Where this form's values are in the spec (`steps.2.config`), for `problems`. */
  at: string;
  sample?: (path: string) => string | undefined;
  /** What the last test rendered this form's config to. */
  preview?: Record<string, unknown> | null;
  secrets: readonly string[];
}

type Obj = Record<string, unknown>;

/**
 * A schema's fields in the order to fill them: the required ones first, as the schema lists them
 * (`properties` arrive alphabetical: the server's JSON keeps no order), then the rest.
 */
export function fieldOrder(schema: Schema): [string, Schema][] {
  const req = schema.required ?? [];
  const rank = (k: string) => (req.includes(k) ? req.indexOf(k) : req.length);
  return Object.entries(schema.properties ?? {}).sort(([a], [b]) => rank(a) - rank(b));
}

/** Whether the field a `Label` titles must be filled. */
const RequiredCtx = createContext(false);

export function ConfigForm({ schema, value, onChange, ctx }: { schema: Schema; value: Obj; onChange: (next: Obj) => void; ctx: FormContext }) {
  const props = fieldOrder(schema);
  const hasModel = props.some(([, s]) => s.format === "roll-model");
  const set = (k: string, v: unknown) => onChange({ ...value, [k]: v });
  if (!props.length) return <p className="text-[12px] text-subtle">Nothing to set up.</p>;
  return (
    <div className="space-y-3.5">
      {props.map(([k, s]) => {
        if (hasModel && (k === "provider" || k === "effort")) return null;
        const at = `${ctx.at}.${k}`;
        return (
          <RequiredCtx.Provider key={k} value={!!schema.required?.includes(k)}>
            <FieldFor
              name={k}
              schema={s}
              required={!!schema.required?.includes(k)}
              value={value[k]}
              onChange={(v) => set(k, v)}
              whole={value}
              onWhole={onChange}
              ctx={{ ...ctx, at }}
              preview={ctx.preview?.[k]}
            />
          </RequiredCtx.Provider>
        );
      })}
    </div>
  );
}

function Label({ title, description, children, inline, htmlFor }: { title: string; description?: string; children: ReactNode; inline?: boolean; htmlFor?: string }) {
  const required = useContext(RequiredCtx);
  const mark = required && (
    <span aria-hidden className="ml-1.5 text-[11px] font-normal text-subtle">
      Required
    </span>
  );
  if (inline)
    return (
      <div className="flex items-center gap-3">
        <div className="min-w-0 flex-1">
          <label htmlFor={htmlFor} className="block text-[12.5px] font-[510] text-soft">
            {title}
            {mark}
          </label>
          {description && <p className="mt-0.5 text-[11.5px] leading-snug text-subtle">{description}</p>}
        </div>
        {children}
      </div>
    );
  return (
    <div>
      <div className="mb-1.5">
        <label htmlFor={htmlFor} className="block text-[12.5px] font-[510] text-soft">
          {title}
          {mark}
        </label>
        {description && <p className="mt-0.5 text-[11.5px] leading-snug text-subtle">{description}</p>}
      </div>
      {children}
    </div>
  );
}

const titleOf = (name: string, s: Schema) => s.title ?? name.replace(/_/g, " ").replace(/^./, (c) => c.toUpperCase());
const show = (v: unknown) => (v === undefined || v === null ? "" : typeof v === "string" ? v : JSON.stringify(v));

function FieldFor({
  name,
  schema: s,
  required,
  value,
  onChange,
  whole,
  onWhole,
  ctx,
  preview,
}: {
  name: string;
  schema: Schema;
  required: boolean;
  value: unknown;
  onChange: (v: unknown) => void;
  whole: Obj;
  onWhole: (next: Obj) => void;
  ctx: FormContext;
  preview?: unknown;
}) {
  const title = titleOf(name, s);
  const error = ctx.problems[ctx.at];
  const tplProps = { variables: ctx.variables, sample: ctx.sample, error, preview: preview === undefined ? null : show(preview) };
  switch (s.format) {
    case "template":
    case "template-long":
      return (
        <Label title={title} description={s.description}>
          <TemplateInput label={title} value={show(value)} onChange={(v) => onChange(v === "" && !required ? null : v)} multiline={s.format === "template-long"} rows={name === "prompt" ? 5 : 3} {...tplProps} />
        </Label>
      );
    case "roll-thread":
      return (
        <Label title={title} description={s.description}>
          <ThreadField label={title} value={typeof value === "string" ? value : null} onChange={onChange} variables={ctx.variables} error={error} />
        </Label>
      );
    case "roll-machine":
      return (
        <Label title={title} description={s.description}>
          <MachineField label={title} value={typeof value === "string" ? value : null} onChange={onChange} variables={ctx.variables} error={error} />
        </Label>
      );
    case "lookup":
      return (
        <Label title={title} description={s.description}>
          <LookupField title={title} schema={s} required={required} value={value} onChange={onChange} whole={whole} variables={ctx.variables} error={error} />
        </Label>
      );
    case "json":
      return (
        <Label title={title} description={s.description}>
          <TemplateInput
            label={title}
            value={typeof value === "string" ? value : value == null ? "" : JSON.stringify(value, null, 2)}
            onChange={(v) => {
              if (!v.trim()) return onChange(null);
              try {
                const parsed: unknown = JSON.parse(v);
                if (parsed && typeof parsed === "object") return onChange(parsed);
              } catch {
                // A template, kept as text.
              }
              onChange(v);
            }}
            multiline
            mono
            rows={4}
            placeholder='{"thread": "{{ steps.start.thread }}"}'
            {...tplProps}
          />
        </Label>
      );
    case "roll-model":
      return (
        <Label title={title} description={s.description}>
          <ModelField value={whole} onChange={onWhole} />
        </Label>
      );
    case "roll-target":
      return (
        <Label title={title} description={s.description}>
          <TargetField value={(value as Target | null) ?? null} onChange={onChange} />
        </Label>
      );
    case "roll-project":
      return (
        <Label title={title} description={s.description}>
          <ProjectField value={(value as ProjectRef | null) ?? null} onChange={onChange} error={error} />
        </Label>
      );
    case "roll-secret":
      return (
        <Label title={title} description={s.description}>
          <SizedSelect width="w-72 max-w-full"
            label={title}
            value={(value as string | null) ?? "__none"}
            onChange={(v) => onChange(v === "__none" ? null : v)}
            options={[{ value: "__none", label: "Don't sign" }, ...ctx.secrets.map((n) => ({ value: n, label: n }))]}
           
          />
          {!ctx.secrets.length && <p className="mt-1 text-[11.5px] text-subtle">Add a secret in Settings to sign requests.</p>}
        </Label>
      );
  }
  if (s.type === "boolean")
    return (
      <Label title={title} description={s.description} inline>
        <Switch checked={value === true || (value == null && s.default === true)} onCheckedChange={(on) => onChange(on)} aria-label={title} />
      </Label>
    );
  if (s.enum && (s.type === "string" || !s.type)) {
    const options = s.enum.map((v, i) => ({ value: v, label: s.enumTitles?.[i] ?? v }));
    const cur = (value as string | null) ?? null;
    return (
      <Label title={title} description={s.description}>
        {options.length <= 3 && options.every((o) => o.label.length <= 16) ? (
          <Segmented label={title} value={cur ?? (s.default as string) ?? "__default"} onChange={(v) => onChange(v === "__default" ? null : v)} options={[...(required || s.default !== undefined ? [] : [{ value: "__default", label: "Default" }]), ...options]} />
        ) : (
          <SizedSelect width="w-72 max-w-full" label={title} value={cur ?? "__default"} onChange={(v) => onChange(v === "__default" ? null : v)} options={[...(required ? [] : [{ value: "__default", label: "Default" }]), ...options]} />
        )}
        {error && <p className="mt-1 text-[11.5px] text-danger">{error}</p>}
      </Label>
    );
  }
  if (s.type === "integer" || s.type === "number")
    return (
      <Label title={title} description={s.description}>
        <NumberField
          optional
          integer={s.type === "integer"}
          aria-label={title}
          value={typeof value === "number" ? value : null}
          min={s.minimum}
          max={s.maximum}
          placeholder={s.default !== undefined ? String(s.default) : "Not set"}
          onChange={onChange}
          className={cn(fieldClass, "w-40 font-mono text-[12.5px] tabular-nums", error && "border-danger/50")}
        />
        {error && <p className="mt-1 text-[11.5px] text-danger">{error}</p>}
      </Label>
    );
  if (s.type === "array" && s.items?.enum) {
    const picked = new Set(Array.isArray(value) ? (value as string[]) : []);
    return (
      <Label title={title} description={s.description}>
        <Chips options={s.items.enum.map((v, i) => ({ value: v, label: s.items!.enumTitles?.[i] ?? v }))} picked={picked} onChange={(next) => onChange(next)} label={title} />
        {error && <p className="mt-1 text-[11.5px] text-danger">{error}</p>}
      </Label>
    );
  }
  if (s.type === "array" && s.items?.type === "object") {
    const rows = Array.isArray(value) ? (value as Obj[]) : [];
    return (
      <Label title={title} description={s.description}>
        <div className="space-y-2">
          {rows.map((r, i) => (
            <div key={i} className="relative rounded-lg border border-line bg-fg/[0.015] p-3 pr-9">
              <ConfigForm schema={s.items!} value={r ?? {}} onChange={(next) => onChange(rows.map((x, j) => (j === i ? next : x)))} ctx={{ ...ctx, at: `${ctx.at}.${i}`, preview: null }} />
              <button type="button" onClick={() => onChange(rows.filter((_, j) => j !== i))} aria-label={`Remove ${title.toLowerCase()} ${i + 1}`} className="absolute top-2 right-2 grid size-6 place-items-center rounded-md text-subtle hover:bg-fg/[0.06] hover:text-fg">
                <Minus className="size-3.5" />
              </button>
            </div>
          ))}
          <AddButton onClick={() => onChange([...rows, Object.fromEntries(Object.keys(s.items!.properties ?? {}).map((k) => [k, s.items!.properties![k].enum && s.items!.properties![k].title === "Answer" ? "accept" : null]))])}>Add {singular(title)}</AddButton>
        </div>
      </Label>
    );
  }
  if (s.type === "array")
    return (
      <Label title={title} description={s.description}>
        <input
          aria-label={title}
          value={Array.isArray(value) ? (value as string[]).join(", ") : ""}
          onChange={(e) => onChange(e.target.value.split(",").map((x) => x.trim()).filter(Boolean))}
          placeholder="Comma separated"
          className={cn(fieldClass, "w-full")}
        />
      </Label>
    );
  if (s.type === "object" && s.additionalProperties && typeof s.additionalProperties === "object") {
    const entries = Object.entries((value as Record<string, string> | null) ?? {});
    const put = (list: [string, string][]) => onChange(list.length ? Object.fromEntries(list) : null);
    return (
      <Label title={title} description={s.description}>
        <div className="space-y-1.5">
          {entries.map(([k, v], i) => (
            <div key={i} className="flex items-start gap-1.5">
              <input aria-label={`${title} name ${i + 1}`} value={k} onChange={(e) => put(entries.map((x, j) => (j === i ? [e.target.value, x[1]] : x)))} placeholder="Name" className={cn(fieldClass, "w-40 shrink-0 font-mono text-[12px]")} />
              <div className="min-w-0 flex-1">
                <TemplateInput label={`${title} value ${i + 1}`} value={v} onChange={(nv) => put(entries.map((x, j) => (j === i ? [x[0], nv] : x)))} variables={ctx.variables} sample={ctx.sample} mono error={ctx.problems[`${ctx.at}.${k}`]} />
              </div>
              <button type="button" onClick={() => put(entries.filter((_, j) => j !== i))} aria-label={`Remove ${k || "the row"}`} className="mt-1 grid size-6 shrink-0 place-items-center rounded-md text-subtle hover:bg-fg/[0.06] hover:text-fg">
                <Minus className="size-3.5" />
              </button>
            </div>
          ))}
          <AddButton onClick={() => put([...entries, ["", ""]])}>Add {singular(title)}</AddButton>
        </div>
      </Label>
    );
  }
  if (s.type === "object" && s.properties)
    return (
      <Label title={title} description={s.description}>
        <div className="rounded-lg border border-line p-3">
          <ConfigForm schema={s} value={(value as Obj) ?? {}} onChange={onChange} ctx={ctx} />
        </div>
      </Label>
    );
  return (
    <Label title={title} description={s.description}>
      <TemplateInput label={title} value={show(value)} onChange={(v) => onChange(v === "" && !required ? null : v)} {...tplProps} />
    </Label>
  );
}

const singular = (t: string) => t.toLowerCase().replace(/s$/, "");

export function AddButton({ onClick, children }: { onClick: () => void; children: ReactNode }) {
  return (
    <button type="button" onClick={onClick} className="inline-flex h-7 items-center gap-1.5 rounded-md px-2 text-[12.5px] text-muted transition-colors hover:bg-fg/[0.05] hover:text-fg">
      <Plus className="size-3.5" />
      {children}
    </button>
  );
}

export function Chips<V extends string>({ options, picked, onChange, label }: { options: { value: V; label: string }[]; picked: ReadonlySet<V>; onChange: (next: V[]) => void; label: string }) {
  return (
    <div className="flex flex-wrap gap-1.5" role="group" aria-label={label}>
      {options.map((o) => {
        const on = picked.has(o.value);
        return (
          <button
            key={o.value}
            type="button"
            aria-pressed={on}
            onClick={() => onChange(options.map((x) => x.value).filter((v) => (v === o.value ? !on : picked.has(v))))}
            className={cn(
              "h-7 rounded-full border px-2.5 text-[12px] transition-[background-color,border-color,color,scale] duration-150 active:scale-[0.97]",
              on ? "border-accent/40 bg-accent/12 text-fg" : "border-line-2 text-muted hover:border-line-3 hover:text-fg",
            )}
          >
            {o.label}
          </button>
        );
      })}
    </div>
  );
}

// ── roll's pickers ──

/** A value a parent field holds, when it's one thing (not a list of several, not a variable). */
function single(v: unknown): string | null {
  const one = Array.isArray(v) ? (v.length === 1 ? v[0] : null) : v;
  return typeof one === "string" && one && !one.includes("{{") ? one : null;
}

/** Linear's kinds of workflow state: a row of one shows the state's icon. */
const STATE_KINDS = new Set(["triage", "backlog", "unstarted", "started", "completed", "canceled"]);

/**
 * A `format: "lookup"` field: the integration's things in the shared picker (one, or a list for an
 * array), its own `extras` first, a typed value or a variable when nothing fits.
 */
function LookupField({ title, schema: s, required, value, onChange, whole, variables, error }: { title: string; schema: Schema; required: boolean; value: unknown; onChange: (v: unknown) => void; whole: Obj; variables: readonly Variable[]; error?: string }) {
  const l = s.lookup ?? {};
  const extras = useMemo(() => (l.extras ?? []).map((e): PickerOption => ({ id: e.id, label: e.label, icon: STATE_KINDS.has(e.id) ? e.id : undefined })), [l.extras]);
  const parent = l.parent ? single(whole[l.parent]) : null;
  const common = { label: title, extras, extrasGroup: l.extras_group, custom: true, variables, error, placeholder: required ? undefined : "Not set" };
  const from = l.source && l.kind ? { source: l.source, kind: l.kind } : null;
  if (s.type === "array") {
    const props = { ...common, multiple: true as const, value: Array.isArray(value) ? value.map(String) : [], onChange: (v: string[]) => onChange(v.length ? v : null) };
    return from ? <LookupPicker {...props} from={from} parent={parent} create={l.create} /> : <Picker {...props} />;
  }
  const props = { ...common, value: value == null || value === "" ? null : String(value), onChange: (v: string | null) => onChange(v), clearable: !required };
  return from ? <LookupPicker {...props} from={from} parent={parent} create={l.create} /> : <Picker {...props} />;
}

/** A roll thread: the owner's threads by title, grouped by project. Empty: the run's last. */
function ThreadField({ label, value, onChange, variables, error }: { label: string; value: string | null; onChange: (v: string | null) => void; variables: readonly Variable[]; error?: string }) {
  const { client } = useAgents();
  const { threads, projects, machines } = client.data;
  const options = useMemo(
    () =>
      [...threads.values()]
        .sort((a, b) => b.value.updated_at - a.value.updated_at)
        .slice(0, 200)
        .map((r): PickerOption => ({ id: r.value.id, label: r.value.title || "Untitled", group: projects.get(r.value.project)?.value.name ?? "Threads", hint: machines.get(r.machine)?.value.name })),
    [threads, projects, machines],
  );
  return <Picker label={label} value={value} onChange={onChange} options={options} custom variables={variables} clearable placeholder="The thread this run started last" error={error} mono />;
}

/** A machine (or sandbox) of the owner's. Empty: the step's default. */
function MachineField({ label, value, onChange, variables, error }: { label: string; value: string | null; onChange: (v: string | null) => void; variables: readonly Variable[]; error?: string }) {
  const { client } = useAgents();
  const options = useMemo(
    () => [...client.data.machines.values()].map((r) => r.value).map((m): PickerOption => ({ id: m.id, label: m.name, hint: m.online ? "online" : m.sandbox ? "asleep" : "offline", group: m.sandbox ? "Sandboxes" : "Machines" })),
    [client.data.machines],
  );
  return <Picker label={label} value={value} onChange={onChange} options={options} custom variables={variables} clearable placeholder="Default" error={error} mono />;
}

/** Every provider of the owner's machines (one machine's when given), the first of each id. */
function useProviders(machine?: string | null): ProviderInfo[] {
  const { client } = useAgents();
  const machines = client.data.machines;
  return useMemo(() => {
    const out = new Map<string, ProviderInfo>();
    for (const r of machines.values()) {
      if (machine && r.value.id !== machine) continue;
      for (const p of r.value.info?.providers ?? []) if (p.installed && !out.has(p.id)) out.set(p.id, p);
    }
    return [...out.values()];
  }, [machines, machine]);
}

function ModelField({ value, onChange }: { value: Obj; onChange: (next: Obj) => void }) {
  const target = value.target as Target | null | undefined;
  const providers = useProviders(target?.type === "machine" ? target.machine : null);
  const all = useProviders(null);
  const list = providers.length ? providers : all;
  const chosen = {
    provider: (value.provider as string) ?? list[0]?.id ?? "",
    model: (value.model as string | null) ?? null,
    effort: (value.effort as string | null) ?? null,
    options: (value.options as ModelOptions | null) ?? {},
  };
  if (!list.length) return <p className="text-[12px] text-subtle">Connect a machine to pick a model; until then it uses the machine&apos;s default.</p>;
  return (
    <div className="inline-flex items-center gap-1 rounded-lg border border-line-2 bg-panel-2 px-1 py-0.5">
      <ModelPicker
        providers={list}
        value={chosen}
        side="bottom"
        onChange={(next) => onChange({ ...value, provider: next.provider, model: next.model, effort: next.effort, options: Object.keys(next.options).length ? next.options : null })}
      />
      {value.provider != null && (
        <button type="button" onClick={() => onChange({ ...value, provider: null, model: null, effort: null, options: null })} className="h-6 rounded-md px-1.5 text-[11.5px] text-subtle hover:bg-fg/[0.06] hover:text-fg">
          Use the project&apos;s default
        </button>
      )}
    </div>
  );
}

const TARGETS: { value: Target["type"] | "default"; label: string }[] = [
  { value: "default", label: "Default" },
  { value: "machine", label: "A machine" },
  { value: "sandbox", label: "New sandbox" },
  { value: "any_online", label: "Any online" },
  { value: "project_machine", label: "Project's" },
];

function TargetField({ value, onChange }: { value: Target | null; onChange: (v: Target | null) => void }) {
  const { client, sandboxes } = useAgents();
  const machines = [...client.data.machines.values()].map((r) => r.value);
  const sizes = sandboxes?.list?.sizes.map((s) => s.name) ?? ["standard", "large", "xl"];
  const kind = value?.type ?? "default";
  return (
    <div className="space-y-2">
      <Segmented
        label="Where it runs"
        value={kind}
        onChange={(k) => {
          if (k === "default") onChange(null);
          else if (k === "machine") onChange({ type: "machine", machine: machines.find((m) => m.online)?.id ?? machines[0]?.id ?? "" });
          else if (k === "sandbox") onChange({ type: "sandbox", size: sizes[0] ?? "standard" });
          else onChange({ type: k });
        }}
        options={TARGETS}
      />
      {value?.type === "machine" && (
        <SizedSelect width="w-72 max-w-full"
          label="Machine"
          value={value.machine || null}
          onChange={(m) => onChange({ type: "machine", machine: m })}
          options={machines.map((m) => ({ value: m.id, label: m.name, hint: m.online ? "online" : m.sandbox ? "asleep" : "offline" }))}
          placeholder="Pick a machine"
         
        />
      )}
      {value?.type === "sandbox" && (
        <div className="flex flex-wrap items-center gap-3">
          <SizedSelect width="w-40" label="Sandbox size" value={value.size} onChange={(size) => onChange({ ...value, size })} options={sizes.map((s) => ({ value: s, label: s[0].toUpperCase() + s.slice(1) }))} />
          <label className="flex items-center gap-2 text-[12.5px] text-soft">
            <Switch size="sm" checked={!!value.keep} onCheckedChange={(keep) => onChange({ ...value, keep: keep || undefined })} aria-label="Keep the sandbox" />
            Keep it after the run
          </label>
        </div>
      )}
      <p className="text-[11.5px] text-subtle">
        {kind === "default"
          ? "The automation's default, else the project's machine."
          : kind === "sandbox"
            ? value?.type === "sandbox" && value.keep
              ? "A fresh sandbox that stays when the run ends."
              : "A fresh sandbox, deleted when the run ends."
            : kind === "any_online"
              ? "Any online machine that has the project (it's cloned if none does)."
              : kind === "project_machine"
                ? "The machine the project is on."
                : "Only this machine; the run waits while it's offline."}
      </p>
    </div>
  );
}

function ProjectField({ value, onChange, error }: { value: ProjectRef | null; onChange: (v: ProjectRef | null) => void; error?: string }) {
  const { client } = useAgents();
  const projects = [...client.data.projects.values()];
  const machineName = (id: string) => client.data.machines.get(id)?.value.name ?? "";
  const kind = value?.type ?? "default";
  return (
    <div className="space-y-2">
      <Segmented
        label="Project by"
        value={kind}
        onChange={(k) => onChange(k === "default" ? null : k === "id" ? { type: "id", id: projects[0]?.value.id ?? "" } : { type: "repo", repo: projects.find((p) => p.value.repo)?.value.repo ? `${projects.find((p) => p.value.repo)!.value.repo!.owner}/${projects.find((p) => p.value.repo)!.value.repo!.repo}` : "", path: null })}
        options={[
          { value: "default", label: "Default" },
          { value: "id", label: "Project" },
          { value: "repo", label: "Repository" },
        ]}
      />
      {value?.type === "id" && (
        <SizedSelect width="w-80 max-w-full"
          label="Project"
          value={value.id || null}
          onChange={(id) => onChange({ type: "id", id })}
          options={projects.map((r) => ({ value: r.value.id, label: r.value.name, hint: machineName(r.machine) }))}
          placeholder="Pick a project"
         
        />
      )}
      {value?.type === "repo" && (
        <div className="flex flex-wrap gap-1.5">
          <LookupPicker label="Repository" from={{ source: "code", kind: "repos" }} value={value.repo || null} onChange={(repo) => onChange({ ...value, repo: repo ?? "" })} placeholder="Pick a repo" custom className="w-56" />
          <input aria-label="Folder" value={value.path ?? ""} onChange={(e) => onChange({ ...value, path: e.target.value || null })} placeholder="Folder (optional)" className={cn(fieldClass, "w-44 font-mono text-[12px]")} />
        </div>
      )}
      {value?.type === "repo" && <p className="text-[11.5px] text-subtle">Cloned onto the machine if it isn&apos;t there yet.</p>}
      {error && <p className="text-[11.5px] text-danger">{error}</p>}
    </div>
  );
}
