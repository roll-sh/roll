import { AlertTriangle, Plus, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { isProduction, previewsApi, type PreviewSettings, type ResourceRule, type SettingsPatch } from "@/lib/previews-api";
import type { PlacedResource } from "@/lib/workspace-api";
import { KIND_LABEL, resourceKey } from "@/lib/workspace-resources";
import { Dialog } from "../agents/ui/dialog";
import { errorText } from "./previews-common";

const field = "rounded-lg border border-line-2 bg-canvas px-2.5 outline-none transition-colors focus:border-accent/60";
const GIB = 1024 ** 3;

/** The project's preview rules (docs/previews.md `Settings`): saved as a patch of what changed. */
export function PreviewSettingsDialog({
  projectId,
  resources,
  names,
  settings,
  open,
  onClose,
  onSaved,
}: {
  projectId: string;
  /** The project's data resources (not repos). */
  resources: PlacedResource[];
  names: Map<string, string>;
  settings: PreviewSettings | null;
  open: boolean;
  onClose: () => void;
  onSaved: (s: PreviewSettings) => void;
}) {
  const [key, setKey] = useState(0);
  return (
    <Dialog
      open={open}
      onClose={onClose}
      onExited={() => setKey((k) => k + 1)}
      title="Preview rules"
      lead="Which changes get a preview, how their data is branched, and when they sleep and go."
      className="w-[min(600px,100%)]"
    >
      {settings ? (
        <Form key={key} projectId={projectId} resources={resources} names={names} saved={settings} onDone={onClose} onSaved={onSaved} />
      ) : (
        <p className="text-[12.5px] text-subtle">Loading…</p>
      )}
    </Dialog>
  );
}

/** The fields that changed, as the API's patch (`resources` and `env` go whole). */
export function settingsPatch(a: PreviewSettings, b: PreviewSettings): SettingsPatch {
  const patch: Record<string, unknown> = {};
  for (const k of Object.keys(b) as (keyof PreviewSettings)[]) if (JSON.stringify(a[k]) !== JSON.stringify(b[k])) patch[k] = b[k];
  return patch as SettingsPatch;
}

function Form({
  projectId,
  resources,
  names,
  saved,
  onDone,
  onSaved,
}: {
  projectId: string;
  resources: PlacedResource[];
  names: Map<string, string>;
  saved: PreviewSettings;
  onDone: () => void;
  onSaved: (s: PreviewSettings) => void;
}) {
  const [s, setS] = useState(saved);
  const [branches, setBranches] = useState(saved.branches.join(", "));
  const [env, setEnv] = useState<[string, string][]>(Object.entries(saved.env));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const set = <K extends keyof PreviewSettings>(k: K, v: PreviewSettings[K]) => setS((x) => ({ ...x, [k]: v }));

  useEffect(() => setError(null), [s, branches, env]);

  const next: PreviewSettings = {
    ...s,
    branches: branches
      .split(/[\s,]+/)
      .map((b) => b.trim())
      .filter(Boolean),
    env: Object.fromEntries(env.filter(([k]) => k.trim()).map(([k, v]) => [k.trim(), v])),
  };
  const patch = settingsPatch(saved, next);
  const changed = Object.keys(patch).length > 0;
  const prod = isProduction(s.secrets.base);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      onSaved(await previewsApi.putSettings(projectId, patch));
      onDone();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      noValidate
      className="space-y-5 text-[12.5px]"
      onSubmit={(e) => {
        e.preventDefault();
        if (changed && !busy) void save();
      }}
    >
      <Toggle label="Previews for this project" note="A change or pull request in its repos gets one when it matches the rules below." checked={s.enabled} onChange={(v) => set("enabled", v)} />

      <Section title="Which changes">
        <Field label="Branches" note="Globs on the change's branch, separated by commas. * is every branch.">
          <input value={branches} onChange={(e) => setBranches(e.target.value)} className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
        </Field>
        <Toggle label="Draft changes too" checked={s.drafts} onChange={(v) => set("drafts", v)} />
      </Section>

      <Section title="Data">
        <Field group label="By default" note="A branch has every row; schema only has the structure and no rows.">
          <Choice value={s.data} options={["branch", "schema"]} onChange={(v) => set("data", v)} labels={{ branch: "Branch", schema: "Schema only" }} />
        </Field>
        {resources.length > 0 && (
          <div className="overflow-hidden rounded-lg border border-line-2">
            {resources.map((r) => {
              const id = resourceKey(r);
              const rule = s.resources[id];
              return (
                <div key={id} className="flex items-center gap-2 border-t border-line px-2.5 py-1.5 first:border-t-0">
                  <span className="min-w-0 flex-1 truncate">
                    {names.get(id) ?? r.ref} <span className="text-[11px] text-subtle">{KIND_LABEL[r.kind]}</span>
                  </span>
                  <select
                    aria-label={`${names.get(id) ?? r.ref} in previews`}
                    value={rule ?? ""}
                    onChange={(e) => {
                      const v = e.target.value as ResourceRule | "";
                      const map = { ...s.resources };
                      if (v) map[id] = v;
                      else delete map[id];
                      set("resources", map);
                    }}
                    className={`${field} h-7 text-[12px]`}
                  >
                    <option value="">Default ({s.data === "branch" ? "branch" : "schema only"})</option>
                    <option value="branch">Branch</option>
                    <option value="schema">Schema only</option>
                    <option value="skip">Skip</option>
                  </select>
                </div>
              );
            })}
          </div>
        )}
        <div className="grid grid-cols-2 gap-3">
          <Field label="Reset every" note="Hours; 0 never.">
            <Num value={s.reset_every_hours} onChange={(v) => set("reset_every_hours", v)} />
          </Field>
          <Field label="Most data per preview" note="GiB its branches may own.">
            <Num value={+(s.max_bytes / GIB).toFixed(2)} step="any" onChange={(v) => set("max_bytes", Math.round(v * GIB))} />
          </Field>
        </div>
      </Section>

      <Section title="Lifetime">
        <div className="grid grid-cols-3 gap-3 max-sm:grid-cols-1">
          <Field label="Sleep after" note="Minutes without a request.">
            <Num value={+(s.sleep_after_s / 60).toFixed(1)} step="any" onChange={(v) => set("sleep_after_s", Math.round(v * 60))} />
          </Field>
          <Field label="Delete after" note="Hours without a push or visit.">
            <Num value={s.ttl_hours} onChange={(v) => set("ttl_hours", v)} />
          </Field>
          <Field label="At most" note="Previews at once; more wait.">
            <Num value={s.max_previews} onChange={(v) => set("max_previews", v)} />
          </Field>
        </div>
      </Section>

      <Section title="Secrets">
        <Field label="Inherit from" note="The secrets environment a preview's starts from.">
          <input value={s.secrets.base} onChange={(e) => set("secrets", { ...s.secrets, base: e.target.value })} className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
        </Field>
        <Toggle
          label="Allow production's secrets"
          note="Off unless you need it. A fork's preview never gets them."
          checked={s.secrets.allow_production}
          onChange={(v) => set("secrets", { ...s.secrets, allow_production: v })}
        />
        {(s.secrets.allow_production || prod) && (
          <p role="alert" className="flex gap-2 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-2 text-[11.5px] text-danger">
            <AlertTriangle className="mt-px size-3.5 shrink-0" strokeWidth={1.5} />
            {s.secrets.allow_production
              ? "Every preview's code runs with production's secrets: anyone who can push a branch can read them."
              : `"${s.secrets.base}" is production's environment: previews inherit it only if you allow production's secrets.`}
          </p>
        )}
        <Field group label="Variables" note="Every service gets these, over the inherited ones.">
          <div className="space-y-1.5">
            {env.map(([k, v], i) => (
              <div key={i} className="flex gap-1.5">
                <input
                  aria-label="Name"
                  placeholder="NAME"
                  value={k}
                  onChange={(e) => setEnv(env.map((x, j) => (j === i ? [e.target.value.toUpperCase(), x[1]] : x)))}
                  className={`${field} h-8 w-2/5 min-w-0 font-mono text-[12px]`}
                  spellCheck={false}
                />
                <input
                  aria-label="Value"
                  placeholder="value"
                  value={v}
                  onChange={(e) => setEnv(env.map((x, j) => (j === i ? [x[0], e.target.value] : x)))}
                  className={`${field} h-8 min-w-0 flex-1 font-mono text-[12px]`}
                  spellCheck={false}
                />
                <button type="button" aria-label="Remove" onClick={() => setEnv(env.filter((_, j) => j !== i))} className="grid size-8 shrink-0 place-items-center rounded-md text-muted hover:bg-white/[0.05] hover:text-fg">
                  <X className="size-3.5" />
                </button>
              </div>
            ))}
            <button type="button" onClick={() => setEnv([...env, ["", ""]])} className="flex h-7 items-center gap-1 rounded-md px-1.5 text-[12px] text-muted hover:bg-white/[0.05] hover:text-fg">
              <Plus className="size-3.5" /> Add a variable
            </button>
          </div>
        </Field>
      </Section>

      <Section title="GitHub pull requests">
        <Toggle label="A comment with the URLs and cost" checked={s.github.comment} onChange={(v) => set("github", { ...s.github, comment: v })} />
        <Toggle label="A commit status (roll/preview)" checked={s.github.status} onChange={(v) => set("github", { ...s.github, status: v })} />
        <Toggle label="A deployment" checked={s.github.deployment} onChange={(v) => set("github", { ...s.github, deployment: v })} />
      </Section>

      {error && <p className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">{error}</p>}
      <div className="flex justify-end gap-2">
        <button type="button" onClick={onDone} className="h-8 rounded-lg px-3 text-[12.5px] text-muted hover:bg-white/[0.05] hover:text-fg">
          Cancel
        </button>
        <button type="submit" disabled={!changed || busy} className="h-8 rounded-lg bg-fg px-3.5 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40">
          {busy ? "Saving…" : "Save"}
        </button>
      </div>
    </form>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="space-y-3 border-t border-line pt-4">
      <h3 className="text-[11.5px] font-medium text-subtle">{title}</h3>
      {children}
    </section>
  );
}

/** A labelled field; `group` for one that holds several controls (a label would click the first). */
function Field({ label, note, group, children }: { label: string; note?: string; group?: boolean; children: ReactNode }) {
  const Tag = group ? "div" : "label";
  return (
    <Tag className="block">
      <span className="mb-1 block text-[12px] font-medium">{label}</span>
      {children}
      {note && <span className="mt-1 block text-[11px] text-subtle">{note}</span>}
    </Tag>
  );
}

function Num({ value, onChange, step = 1 }: { value: number; onChange: (v: number) => void; step?: number | "any" }) {
  return (
    <input
      type="number"
      min={0}
      step={step}
      value={Number.isFinite(value) ? value : 0}
      onChange={(e) => onChange(e.target.value === "" ? 0 : Number(e.target.value))}
      className={`${field} h-8 w-full font-mono text-[12px] tabular-nums`}
    />
  );
}

function Toggle({ label, note, checked, onChange }: { label: string; note?: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className="flex cursor-pointer items-start gap-3">
      <span className="min-w-0 flex-1">
        <span className="block text-[12.5px] font-medium">{label}</span>
        {note && <span className="block text-[11px] text-subtle">{note}</span>}
      </span>
      <input type="checkbox" role="switch" checked={checked} onChange={(e) => onChange(e.target.checked)} className="peer sr-only" />
      <span
        aria-hidden
        className="relative mt-0.5 h-4 w-7 shrink-0 rounded-full border border-line-2 bg-panel-2 transition-colors duration-150 peer-checked:border-accent/60 peer-checked:bg-accent peer-focus-visible:outline-2 peer-focus-visible:outline-offset-2 peer-focus-visible:outline-accent after:absolute after:top-[1px] after:left-[1px] after:size-3 after:rounded-full after:bg-fg after:transition-transform after:duration-150 peer-checked:after:translate-x-3 motion-reduce:transition-none motion-reduce:after:transition-none"
      />
    </label>
  );
}

function Choice<T extends string>({ value, options, labels, onChange }: { value: T; options: T[]; labels: Record<T, string>; onChange: (v: T) => void }) {
  return (
    <span role="radiogroup" className="inline-flex rounded-lg border border-line-2 bg-canvas p-0.5">
      {options.map((o) => (
        <button
          key={o}
          type="button"
          role="radio"
          aria-checked={value === o}
          onClick={() => onChange(o)}
          className={`h-7 rounded-md px-2.5 text-[12px] transition-colors ${value === o ? "bg-panel-3 text-fg" : "text-muted hover:text-fg"}`}
        >
          {labels[o]}
        </button>
      ))}
    </span>
  );
}
