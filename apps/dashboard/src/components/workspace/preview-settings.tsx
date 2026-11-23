import { AlertTriangle, KeyRound, Plus, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { isProduction, parseRouteBudgets, previewsApi, routeBudgetLines, unmaskedNote, type MaskSuggestion, type MaskSuggestions, type PageState, type PreviewBypass, type PreviewHook, type PreviewJob, type PreviewSettings, type ResourceRule, type SettingsPatch } from "@/lib/previews-api";
import { ROLL_API } from "@/lib/roll-api";
import { keepable } from "@/lib/secret-names";
import { needsApproval, secretsApi } from "@/lib/secrets-api";
import type { PlacedResource } from "@/lib/workspace-api";
import { KIND_LABEL, resourceKey } from "@/lib/workspace-resources";
import { Dialog } from "../agents/ui/dialog";
import { errorText } from "./previews-common";

const field = "rounded-lg border border-line-2 bg-canvas px-2.5 outline-none transition-colors focus:border-accent/60";
const GIB = 1024 ** 3;

/** The project's preview rules (docs/previews.md `Settings`): saved as a patch of what changed. */
/** The page states field's JSON (empty: none); anything that isn't a list goes to roll as is,
 * which says what's wrong with it. */
function parseStates(text: string): PageState[] {
  if (!text.trim()) return [];
  try {
    return JSON.parse(text) as PageState[];
  } catch {
    return [{ name: "", page: "", steps: [] }];
  }
}

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

/** A comma- or space-separated list. */
const words = (text: string) =>
  text
    .split(/[\s,]+/)
    .map((w) => w.trim())
    .filter(Boolean);

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
  const budget = s.github.budget ?? { load_ms: 0, bytes: 0 };
  const [branches, setBranches] = useState(saved.branches.join(", "));
  const [paths, setPaths] = useState(saved.paths.join(", "));
  const [ignorePaths, setIgnorePaths] = useState(saved.ignore_paths.join(", "));
  const [skipAuthors, setSkipAuthors] = useState((saved.skip_authors ?? []).join(", "));
  const [egressHosts, setEgressHosts] = useState((saved.egress_hosts ?? []).join(", "));
  const [pages, setPages] = useState((saved.github.pages ?? []).join(", "));
  const [widths, setWidths] = useState((saved.github.widths ?? []).join(", "));
  const [openapi, setOpenapi] = useState(saved.github.openapi ?? "");
  const [endpoints, setEndpoints] = useState((saved.github.endpoints ?? []).join(", "));
  const [routeBudgets, setRouteBudgets] = useState(routeBudgetLines(saved.github.budget?.routes));
  // Selectors can hold commas: one per line.
  const [hide, setHide] = useState((saved.github.hide ?? []).join("\n"));
  const [states, setStates] = useState(saved.github.states?.length ? JSON.stringify(saved.github.states, null, 2) : "");
  const [env, setEnv] = useState<[string, string][]>(Object.entries(saved.env));
  // A variable being kept in roll secrets (its row), and what keeping it said.
  const [keeping, setKeeping] = useState<number | null>(null);
  const [kept, setKept] = useState<string | null>(null);
  const keepInSecrets = async (i: number, key: string, value: string) => {
    const base = s.secrets.base;
    setKeeping(i);
    setKept(null);
    try {
      const api = secretsApi(projectId);
      try {
        await api.set(base, key, { value, message: "kept from the preview rules" });
      } catch (e) {
        // A protected base takes a change request (as roll secrets' own Find opens one): until it's
        // approved the key isn't there, so the rule keeps its value (a reference would read nothing).
        if (!needsApproval(e)) throw e;
        await api.openRequest(base, { path: "/", message: "kept from the preview rules", ops: [{ op: "set", key, value }] });
        setKept(`${base} is protected: a change request for ${key} waits for approval in roll secrets. Use roll://${base}/${key} here once it's approved.`);
        return;
      }
      setEnv((rows) => rows.map((x, j) => (j === i ? [x[0], `roll://${base}/${key}`] : x)));
      setKept(`${key} is in roll secrets (${base}); Save to use it from there.`);
    } catch (e) {
      setError(`Keeping ${key} in roll secrets: ${errorText(e)}`);
    } finally {
      setKeeping(null);
    }
  };
  // `env_for` as rows of (branch glob, name, value).
  const [perBranch, setPerBranch] = useState<[string, string, string][]>(Object.entries(saved.env_for ?? {}).flatMap(([g, vars]) => Object.entries(vars).map(([k, v]) => [g, k, v] as [string, string, string])));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [suggested, setSuggested] = useState<MaskSuggestions>({ suggestions: [], unread: [] });
  // Read only when asked: reading a parent's columns resets a running database's idle timer.
  const [looking, setLooking] = useState<"no" | "reading" | "done">("no");
  const set = <K extends keyof PreviewSettings>(k: K, v: PreviewSettings[K]) => setS((x) => ({ ...x, [k]: v }));
  // A mask added from a suggestion joins the masks being edited, saved with the rest.
  const addMasks = (resource: string, add: MaskSuggestion[]) =>
    setS((x) => ({ ...x, masking: { ...x.masking, [resource]: { ...x.masking?.[resource], ...Object.fromEntries(add.map((m) => [m.path, m.rule])) } } }));

  // Only a hint: the dialog works the same without it.
  const lookForPersonalData = () => {
    setLooking("reading");
    previewsApi.maskSuggestions(projectId).then(
      (r) => {
        setSuggested(r);
        setLooking("done");
      },
      () => setLooking("done"),
    );
  };

  useEffect(() => setError(null), [s, branches, paths, ignorePaths, skipAuthors, pages, widths, openapi, endpoints, routeBudgets, states, hide, env, perBranch]);

  const next: PreviewSettings = {
    ...s,
    branches: words(branches),
    paths: words(paths),
    ignore_paths: words(ignorePaths),
    skip_authors: words(skipAuthors),
    egress_hosts: s.egress === "roll" || (s.egress_forks ?? "roll") === "roll" ? words(egressHosts) : [],
    github: {
      ...s.github,
      pages: words(pages),
      widths: words(widths).map(Number),
      openapi: openapi.trim() || null,
      endpoints: words(endpoints),
      budget: { ...(s.github.budget ?? { load_ms: 0, bytes: 0 }), routes: parseRouteBudgets(routeBudgets).routes },
      states: parseStates(states),
      hide: hide
        .split("\n")
        .map((x) => x.trim())
        .filter(Boolean),
    },
    env: Object.fromEntries(env.filter(([k]) => k.trim()).map(([k, v]) => [k.trim(), v])),
    env_for: perBranch
      .filter(([g, k]) => g.trim() && k.trim())
      .reduce<Record<string, Record<string, string>>>((m, [g, k, v]) => ({ ...m, [g.trim()]: { ...(m[g.trim()] ?? {}), [k.trim()]: v } }), {}),
  };
  const patch = settingsPatch(saved, next);
  const changed = Object.keys(patch).length > 0;
  const prod = isProduction(s.secrets.base);

  const save = async () => {
    const unread = parseRouteBudgets(routeBudgets).bad;
    if (unread.length) {
      setError(`Route budgets: a line is a route then its milliseconds (GET /orders 300); not ${unread.map((l) => `"${l}"`).join(", ")}`);
      return;
    }
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
        <Toggle label="Only when asked" note="Only changes with [preview] in their title get one." checked={s.manual} onChange={(v) => set("manual", v)} />
        <Field label="Paths" note="A change must touch one of these, gitignore-style (apps/web/, *.ts). Empty is any path.">
          <input value={paths} onChange={(e) => setPaths(e.target.value)} placeholder="any path" className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
        </Field>
        <Field label="Ignored paths" note="Touching only these doesn't count (docs/, *.md).">
          <input value={ignorePaths} onChange={(e) => setIgnorePaths(e.target.value)} className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
        </Field>
        <Field label="Skipped authors" note="Globs on who opened it (dependabot[bot], *[bot] for every bot account).">
          <input value={skipAuthors} onChange={(e) => setSkipAuthors(e.target.value)} placeholder="nobody" className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
        </Field>
        <p className="text-muted">
          A change's title overrides these: <span className="font-mono text-[12px]">[skip preview]</span> skips it, <span className="font-mono text-[12px]">[preview]</span> asks for one.
        </p>
      </Section>

      <Section title="Data">
        <Field group label="By default" note="A branch has every row; schema only has the structure and no rows.">
          <Choice value={s.data} options={["branch", "schema"]} onChange={(v) => set("data", v)} labels={{ branch: "Branch", schema: "Schema only" }} />
        </Field>
        {resources.length > 0 && looking !== "done" && (
          <p className="text-[12px] text-muted">
            <button type="button" disabled={looking === "reading"} onClick={lookForPersonalData} className="text-accent hover:underline disabled:opacity-50">
              {looking === "reading" ? "Reading the columns…" : "Look for personal data that isn't masked"}
            </button>{" "}
            <span className="text-subtle">(reads each database's columns; a sleeping one isn't woken)</span>
          </p>
        )}
        {resources.length > 0 && (
          <div className="overflow-hidden rounded-lg border border-line-2">
            {resources.map((r) => {
              const id = resourceKey(r);
              const rule = s.resources[id];
              // Structure-only exists for these; for the rest a `schema` rule is refused.
              const schema = r.kind === "postgres" || r.kind === "nosql" || r.kind === "stream" || r.kind === "queue" || r.kind === "tasks";
              const masks = Object.entries(s.masking?.[id] ?? {})
                .map(([col, f]) => `${col} = ${f}`)
                .join("\n");
              const mode = rule ?? (s.data === "branch" ? "branch" : schema ? "schema" : "skip");
              const unmasked = unmaskedNote(r.kind, mode, Object.keys(s.masking?.[id] ?? {}).length);
              // Read for every resource, so one switched to "branch" here shows them at once.
              const suggestions = mode === "branch" ? suggested.suggestions.filter((x) => x.resource === id && !s.masking?.[id]?.[x.path]) : [];
              const unread = mode === "branch" ? suggested.unread.find((x) => x.resource === id) : undefined;
              return (
                <div key={id} className="border-t border-line first:border-t-0">
                  <div className="flex items-center gap-2 px-2.5 py-1.5">
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
                      <option value="">Default ({s.data === "branch" ? "branch" : schema ? "schema only" : "skipped"})</option>
                      <option value="branch">Branch</option>
                      {schema && <option value="schema">Schema only</option>}
                      <option value="skip">Skip</option>
                    </select>
                  </div>
                  {unmasked && <p className="px-2.5 pb-1.5 text-[11px] text-soft">{unmasked}</p>}
                  {(r.kind === "nosql" || r.kind === "postgres" || r.kind === "search") && (
                    <div className="px-2.5 pb-2">
                      <textarea
                        aria-label={`Masked ${r.kind === "search" ? "fields" : "columns"} of ${names.get(id) ?? r.ref}`}
                        key={masks}
                        defaultValue={masks}
                        placeholder={r.kind === "postgres" ? "public.users.email = email" : r.kind === "search" ? "users.email = email" : "app.users.email = mask_default()"}
                        rows={Math.max(1, masks.split("\n").length)}
                        onBlur={(e) => {
                          const cols: Record<string, string> = {};
                          for (const line of e.target.value.split("\n")) {
                            const [col, ...f] = line.split("=");
                            if (col?.trim() && f.join("=").trim()) cols[col.trim()] = f.join("=").trim();
                          }
                          const map = { ...(s.masking ?? {}) };
                          if (Object.keys(cols).length) map[id] = cols;
                          else delete map[id];
                          set("masking", map);
                        }}
                        className={`${field} w-full resize-y py-1.5 font-mono text-[11px]`}
                        spellCheck={false}
                      />
                      <p className="mt-1 text-[11px] text-subtle">
                        {r.kind === "postgres"
                          ? "Rewritten in each preview's branch before anything serves it, one column per line: schema.table.column = null, redact, hash, email or fixed:<value>. The parent is untouched."
                          : r.kind === "search"
                            ? "Rewritten in each preview's branch before anything serves it, one field per line: index.field (nested: index.a.b) = redact, remove or email. The parent is untouched."
                            : "Masked in previews, one column per line: keyspace.table.column = mask function. The parent is untouched."}
                      </p>
                      {suggestions.length > 0 && <MaskHints items={suggestions} onAdd={(add) => addMasks(id, add)} />}
                      {unread && <p className="mt-1.5 text-[11px] text-subtle">Mask suggestions: {unread.why}</p>}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
        <Field label="Data as of" note="Branch the data as it was then, where the product keeps history (Postgres, NoSQL): -24h, -30m, -7d or a time. Empty: now.">
          <input
            value={s.data_at ?? ""}
            onChange={(e) => set("data_at", e.target.value.trim() || null)}
            placeholder="now"
            className={`${field} h-8 w-full font-mono text-[12px]`}
            spellCheck={false}
          />
        </Field>
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
        <Field label="Monthly budget" note="USD the project's previews may cost in a month; once reached, new previews wait. 0 is no cap.">
          <Num value={s.max_usd_month ?? 0} step="any" onChange={(v) => set("max_usd_month", v)} />
        </Field>
        <Toggle label="Warm after each push" note="Its services start as soon as a push is deployed, so the pull request's link opens at once." checked={s.warm} onChange={(v) => set("warm", v)} />
        <Toggle label="Run crons on their schedule" note="Off: a preview's crons and scheduled commands are deployed, but their schedule doesn't fire, so a preview nobody looks at runs no jobs and mails no one." checked={!!s.crons} onChange={(v) => set("crons", v)} />
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
                  placeholder="value or roll://ENV/KEY"
                  value={v}
                  onChange={(e) => setEnv(env.map((x, j) => (j === i ? [x[0], e.target.value] : x)))}
                  className={`${field} h-8 min-w-0 flex-1 font-mono text-[12px]`}
                  spellCheck={false}
                />
                {/* A credential pasted here would sit in the rules as text: offered a key in the
                    base environment instead, the rule becoming its reference. */}
                {keepable(k, v) && (
                  <button
                    type="button"
                    aria-label={`Keep ${k} in roll secrets`}
                    title={`Keep it in roll secrets (${s.secrets.base}), and use roll://${s.secrets.base}/${k} here`}
                    disabled={keeping === i}
                    onClick={() => void keepInSecrets(i, k.trim(), v)}
                    className="grid size-8 shrink-0 place-items-center rounded-md text-accent hover:bg-white/[0.05] disabled:opacity-50"
                  >
                    <KeyRound className="size-3.5" strokeWidth={1.5} />
                  </button>
                )}
                <button type="button" aria-label="Remove" onClick={() => setEnv(env.filter((_, j) => j !== i))} className="grid size-8 shrink-0 place-items-center rounded-md text-muted hover:bg-white/[0.05] hover:text-fg">
                  <X className="size-3.5" />
                </button>
              </div>
            ))}
            <button type="button" onClick={() => setEnv([...env, ["", ""]])} className="flex h-7 items-center gap-1 rounded-md px-1.5 text-[12px] text-muted hover:bg-white/[0.05] hover:text-fg">
              <Plus className="size-3.5" /> Add a variable
            </button>
            {kept && <p className="text-[11.5px] text-muted">{kept}</p>}
          </div>
        </Field>
        <Field group label="Per branch" note="For previews of branches a glob matches (feat/payments-*), over the variables above.">
          <div className="space-y-1.5">
            {perBranch.map(([g, k, v], i) => (
              <div key={i} className="flex gap-1.5">
                <input
                  aria-label="Branches"
                  placeholder="feat/*"
                  value={g}
                  onChange={(e) => setPerBranch(perBranch.map((x, j) => (j === i ? [e.target.value, x[1], x[2]] : x)))}
                  className={`${field} h-8 w-1/4 min-w-0 font-mono text-[12px]`}
                  spellCheck={false}
                />
                <input
                  aria-label="Name"
                  placeholder="NAME"
                  value={k}
                  onChange={(e) => setPerBranch(perBranch.map((x, j) => (j === i ? [x[0], e.target.value.toUpperCase(), x[2]] : x)))}
                  className={`${field} h-8 w-1/4 min-w-0 font-mono text-[12px]`}
                  spellCheck={false}
                />
                <input
                  aria-label="Value"
                  placeholder="value"
                  value={v}
                  onChange={(e) => setPerBranch(perBranch.map((x, j) => (j === i ? [x[0], x[1], e.target.value] : x)))}
                  className={`${field} h-8 min-w-0 flex-1 font-mono text-[12px]`}
                  spellCheck={false}
                />
                <button type="button" aria-label="Remove" onClick={() => setPerBranch(perBranch.filter((_, j) => j !== i))} className="grid size-8 shrink-0 place-items-center rounded-md text-muted hover:bg-white/[0.05] hover:text-fg">
                  <X className="size-3.5" />
                </button>
              </div>
            ))}
            <button type="button" onClick={() => setPerBranch([...perBranch, ["", "", ""]])} className="flex h-7 items-center gap-1 rounded-md px-1.5 text-[12px] text-muted hover:bg-white/[0.05] hover:text-fg">
              <Plus className="size-3.5" /> Add one for a branch
            </button>
          </div>
        </Field>
      </Section>

      <Section title="Seed, release and teardown">
        <Job
          label="Seed"
          note="Runs once on a preview's fresh data, before its services start: anonymise rows, load fixtures."
          job={s.seed}
          placeholder={["postgres:18-alpine", 'psql "$DATABASE_URL" -f seed/anonymise.sql']}
          onChange={(j) => set("seed", j)}
        />
        <Job
          label="Release"
          note="Runs at each new head of the change, before its services deploy: migrations. The schema it leaves is compared with the parent's."
          job={s.release ?? null}
          placeholder={["node:22", "npm ci && npm run migrate"]}
          onChange={(j) => set("release", j)}
        />
        <Job
          label="Teardown"
          note="Runs once when a preview goes, before its services and branches: remove what its code made outside roll (test webhooks, OAuth redirect URIs). A failure is logged; the preview goes anyway."
          job={s.teardown ?? null}
          placeholder={["node:22", "node scripts/unregister-webhooks.js"]}
          onChange={(j) => set("teardown", j)}
        />
      </Section>

      <Section title="Who can open them">
        <Field
          group
          label="Access"
          note="Previews run on branches of real data. A private link answers only to the link roll hands out; signed in to roll lets in only people who can open this project, who sign in on the way. Share links work with either."
        >
          <Choice
            value={s.access}
            options={["link", "team", "public"]}
            onChange={(v) => set("access", v)}
            labels={{ link: "Private link", team: "Signed in to roll", public: "Anyone with the URL" }}
          />
        </Field>
        {s.access !== "public" && <Bypass projectId={projectId} />}
        <Hooks projectId={projectId} />
        <Field group label="Mail" note="Previews run on copies of production's people. Caught: their services send mail to roll, where it's listed on the preview and goes no further (mail providers' API keys are removed).">
          <Choice value={s.mail ?? "catch"} options={["catch", "deliver"]} onChange={(v) => set("mail", v)} labels={{ catch: "Caught by roll", deliver: "Delivered" }} />
        </Field>
        <Field group label="Traces" note="Its services get the OpenTelemetry SDK's variables, so an app with an SDK sends its spans to the preview: listed under Requests on its sheet, masked as its logs are, kept a few hours.">
          <Choice value={s.traces ? "on" : "off"} options={["off", "on"]} onChange={(v) => set("traces", v === "on")} labels={{ off: "Off", on: "On" }} />
        </Field>
        <Field group label="Reaches" note="What a preview's code may call. Roll only: its services run on a network of their own that reaches roll's products (its branches) and nothing on the internet, so a webhook or a live payment API can't be called from a change.">
          <Choice value={s.egress ?? "open"} options={["open", "roll"]} onChange={(v) => set("egress", v)} labels={{ open: "Anything", roll: "Roll only" }} />
        </Field>
        {(s.egress ?? "open") === "open" && (
          <Field group label="A fork's preview reaches" note="A pull request from a fork runs only once a writer approves its head, but an approval is to run the code, not a read of what it sends out. Roll only by default: it can't post the preview's values anywhere unless you open it.">
            <Choice value={s.egress_forks ?? "roll"} options={["roll", "open"]} onChange={(v) => set("egress_forks", v)} labels={{ roll: "Roll only", open: "Anything" }} />
          </Field>
        )}
        {(s.egress === "roll" || (s.egress_forks ?? "roll") === "roll") && (
          <Field group label="Allowed outside hosts" note="With roll only (for every preview, or a fork's), the hosts outside roll its code may still reach, through a proxy that refuses anything else (api.stripe.com, or *.sentry.io for its subdomains). What it refused is in the preview's activity.">
            <input value={egressHosts} onChange={(e) => setEgressHosts(e.target.value)} placeholder="api.stripe.com, *.sentry.io" className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
          </Field>
        )}
      </Section>

      <Section title="GitHub pull requests">
        <Toggle label="A comment with the URLs and cost" checked={s.github.comment} onChange={(v) => set("github", { ...s.github, comment: v })} />
        <Toggle label="A commit status (roll/preview)" checked={s.github.status} onChange={(v) => set("github", { ...s.github, status: v })} />
        <Toggle label="A deployment" checked={s.github.deployment} onChange={(v) => set("github", { ...s.github, deployment: v })} />
        <Toggle label="A feedback button on its pages" note="For people who open it through roll: a note pinned to the page posts on the change." checked={s.feedback} onChange={(v) => set("feedback", v)} />
      </Section>

      <Section title="Screenshots and checks">
        <Toggle label="A screenshot of each push" note="The first service's page at desktop and phone widths, timed and checked for errors and accessibility, in roll and the pull request." checked={!!s.github.screenshot} onChange={(v) => set("github", { ...s.github, screenshot: v })} />
        {s.github.screenshot && (
          <Toggle
            label="Compare each change with the default branch (roll keeps a preview of it)"
            checked={s.github.baseline !== false}
            onChange={(v) => set("github", { ...s.github, baseline: v })}
          />
        )}
        {s.github.screenshot && (
          <Field group label="Visual review" note="A change whose pages differ from the default branch's waits for someone to accept it, at each push (the roll/visual status); required also keeps it from landing in roll.">
            <Choice
              value={s.visual_review ?? "off"}
              options={["off", "check", "required"]}
              onChange={(v) => set("visual_review", v)}
              labels={{ off: "Off", check: "A check", required: "Required to land" }}
            />
          </Field>
        )}
        <Field label="Route budgets" note="A route (a trace's root span name) and its p50 in ms, one a line: over it, a route counts as a regression whatever main's does (needs traces).">
          <textarea
            value={routeBudgets}
            onChange={(e) => setRouteBudgets(e.target.value)}
            placeholder={"GET /orders 300\nPOST /checkout 800"}
            rows={3}
            className={`${field} w-full py-1.5 font-mono text-[12px]`}
            spellCheck={false}
          />
        </Field>
        <Field
          group
          label="Regressions against main"
          note="A change whose routes are slower than on the default branch's preview, or whose output has errors that one doesn't, fails the roll/regressions status until they're gone or someone accepts them (routes need traces); required also keeps it from landing in roll."
        >
          <Choice value={s.regressions ?? "off"} options={["off", "check", "required"]} onChange={(v) => set("regressions", v)} labels={{ off: "Off", check: "A check", required: "Required to land" }} />
        </Field>
        <Field
          group
          label="Production's browser checks"
          note="roll apps runs a service's production browser checks once on each preview head. A check posts the roll/browser-checks status (pending while they run, failing while one fails); required also keeps the change from landing in roll until they pass."
        >
          <Choice value={s.browser_checks ?? "report"} options={["report", "check", "required"]} onChange={(v) => set("browser_checks", v)} labels={{ report: "Shown", check: "A check", required: "Required to land" }} />
        </Field>
        {s.github.screenshot && (
          <Field label="Other pages" note="Taken with / at each push and compared the same way (up to 10: /pricing, or another service's, admin:/users).">
            <input value={pages} onChange={(e) => setPages(e.target.value)} placeholder="/ only" className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
          </Field>
        )}
        {s.github.screenshot && (
          <Field label="More widths" note="The first page at these widths too, each compared and reviewed like a page (up to 3: 768 for a tablet, 1920).">
            <input value={widths} onChange={(e) => setWidths(e.target.value)} placeholder="desktop and phone only" className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
          </Field>
        )}
        {s.github.screenshot && (
          <Field label="Endpoints" note="GET paths of the first service's, timed on the preview's data (the median of 10) and compared with main's: up to 5.">
            <input value={endpoints} onChange={(e) => setEndpoints(e.target.value)} placeholder="/api/users, /api/orders" className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
          </Field>
        )}
        {s.github.screenshot && (
          <Field label="API document" note="A path of the first service's serving its OpenAPI document as JSON: its operations are compared with main's, a removed one named as breaking.">
            <input value={openapi} onChange={(e) => setOpenapi(e.target.value)} placeholder="/openapi.json" className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
          </Field>
        )}
        {s.github.screenshot && (
          <div className="grid grid-cols-2 gap-3">
            <Field label="Speed budget (ms)" note="Over it, roll/speed fails. 0 is no limit.">
              <Num value={s.github.budget?.load_ms ?? 0} onChange={(v) => set("github", { ...s.github, budget: { ...budget, load_ms: v } })} />
            </Field>
            <Field label="Size budget (KB)" note="The first page and what it loads.">
              <Num value={Math.round(budget.bytes / 1024)} onChange={(v) => set("github", { ...s.github, budget: { ...budget, bytes: v * 1024 } })} />
            </Field>
            <Field label="LCP budget (ms)" note="Largest contentful paint; Core Web Vitals call up to 2500 good.">
              <Num value={budget.lcp_ms ?? 0} onChange={(v) => set("github", { ...s.github, budget: { ...budget, lcp_ms: v } })} />
            </Field>
            <Field label="Layout shift budget" note="Cumulative layout shift; up to 0.1 is good.">
              <Num value={budget.cls ?? 0} step="any" onChange={(v) => set("github", { ...s.github, budget: { ...budget, cls: v } })} />
            </Field>
          </div>
        )}
        {s.github.screenshot && (
          <Field label="Visual threshold (%)" note="A page counts as changed for the visual review only past this share of its pixels (up to 50). 0: any change counts.">
            <Num value={s.github.visual_threshold ?? 0} step="any" onChange={(v) => set("github", { ...s.github, visual_threshold: Math.min(v, 50) })} />
          </Field>
        )}
        {s.github.screenshot && (
          <Field label="Other browsers" note="The first page in Firefox and WebKit too (and WebKit at a phone's size, mobile Safari's engine), each compared with main's in that browser and reviewed like a page.">
            <div className="flex gap-4">
              {(["firefox", "webkit", "webkit-phone"] as const).map((b) => (
                <label key={b} className="flex items-center gap-1.5 text-[12px] text-soft">
                  <input
                    type="checkbox"
                    checked={(s.github.browsers ?? []).includes(b)}
                    onChange={(e) => set("github", { ...s.github, browsers: e.target.checked ? [...(s.github.browsers ?? []).filter((x) => x !== b), b] : (s.github.browsers ?? []).filter((x) => x !== b) })}
                  />
                  {b === "firefox" ? "Firefox" : b === "webkit" ? "WebKit" : "WebKit at a phone's size"}
                </label>
              ))}
            </div>
          </Field>
        )}
        {s.github.screenshot && (
          <Field label="Page states" note="Pages reached by a few steps (a cart open, a dialog), each screenshotted and reviewed like a page: a JSON list of {name, page, steps}, steps {click: selector}, {type: [selector, text]}, {press: key} or {wait: ms}. Up to 3.">
            <textarea value={states} onChange={(e) => setStates(e.target.value)} rows={3} placeholder='[{"name": "cart", "page": "/", "steps": [{"click": "#open-cart"}]}]' className={`${field} w-full py-1.5 font-mono text-[12px]`} spellCheck={false} />
          </Field>
        )}
        {s.github.screenshot && (
          <Field label="Hidden in screenshots" note="CSS selectors blanked before each screenshot (a clock, an ad), one per line; animations always stop first.">
            <textarea value={hide} onChange={(e) => setHide(e.target.value)} rows={2} placeholder=".clock" className={`${field} w-full py-1.5 font-mono text-[12px]`} spellCheck={false} />
          </Field>
        )}
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

/** Columns that look like people's data and have no mask yet, each with the rule that fits it. */
function MaskHints({ items, onAdd }: { items: MaskSuggestion[]; onAdd: (add: MaskSuggestion[]) => void }) {
  const [all, setAll] = useState(false);
  const shown = all ? items : items.slice(0, 5);
  const add = "flex h-6 shrink-0 items-center gap-1 rounded-md px-1.5 text-[11px] text-accent hover:bg-white/[0.05]";
  return (
    <div className="mt-2 rounded-md border border-line px-2 py-1.5">
      <div className="flex items-center gap-2">
        <p className="min-w-0 flex-1 text-[11px] text-muted">These look like personal data and aren't masked</p>
        {items.length > 1 && (
          <button type="button" onClick={() => onAdd(items)} className={add}>
            Add all
          </button>
        )}
      </div>
      <ul className="mt-0.5">
        {shown.map((x) => (
          <li key={x.path} className="flex items-center gap-2">
            <span className="min-w-0 flex-1 truncate text-[11px] text-subtle" title={`${x.path}: ${x.why}`}>
              <span className="font-mono text-soft">{x.path}</span> · {x.why} · <span className="font-mono">{x.rule}</span>
            </span>
            <button type="button" aria-label={`Add mask ${x.path} = ${x.rule}`} onClick={() => onAdd([x])} className={add}>
              <Plus className="size-3" strokeWidth={1.5} /> Add mask
            </button>
          </li>
        ))}
      </ul>
      {items.length > shown.length && (
        <button type="button" onClick={() => setAll(true)} className="mt-0.5 h-6 rounded-md px-1 text-[11px] text-muted hover:text-fg">
          {items.length - shown.length} more
        </button>
      )}
    </div>
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
/** A seed or release command: its image and command, empty for none. */
function Job({ label, note, job, placeholder, onChange }: { label: string; note: string; job: PreviewJob | null; placeholder: [string, string]; onChange: (j: PreviewJob | null) => void }) {
  const change = (patch: Partial<PreviewJob>) => {
    const next = { ...(job ?? { image: "", run: "" }), ...patch };
    onChange(next.image || next.run ? next : null);
  };
  return (
    <div className="grid gap-2">
      <Field label={`${label} image`} note={note}>
        <input value={job?.image ?? ""} placeholder={placeholder[0]} onChange={(e) => change({ image: e.target.value })} className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
      </Field>
      <Field label={`${label} command`} note="sh -c in the change's tree, with the branches' DATABASE_URL, REDIS_URL and the rest.">
        <input value={job?.run ?? ""} placeholder={placeholder[1]} onChange={(e) => change({ run: e.target.value })} className={`${field} h-8 w-full font-mono text-[12px]`} spellCheck={false} />
      </Field>
    </div>
  );
}

/** The project's bypass secret for CI (acts at once, not on Save): shown once when made. */
function Bypass({ projectId }: { projectId: string }) {
  const [state, setState] = useState<PreviewBypass | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    let live = true;
    previewsApi.bypass(projectId).then((b) => live && setState(b), (e) => live && setError(errorText(e)));
    return () => {
      live = false;
    };
  }, [projectId]);
  const act = async (f: () => Promise<PreviewBypass>) => {
    setBusy(true);
    setError(null);
    try {
      setState(await f());
      setCopied(false);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const header = state?.header ?? "x-roll-preview-bypass";
  const btn = "h-7 rounded-md border border-line-2 px-2.5 text-[12px] text-muted transition-colors hover:text-fg disabled:opacity-50";
  return (
    <Field group label="Automation" note={`End-to-end tests in CI send this secret as the ${header} header to open private previews. Keep it in your CI's secrets.`}>
      {state?.secret ? (
        <span className="flex items-center gap-2">
          <code className={`${field} flex h-8 min-w-0 flex-1 items-center truncate font-mono text-[12px]`}>{state.secret}</code>
          <button
            type="button"
            className={btn}
            onClick={() => navigator.clipboard?.writeText(state.secret ?? "").then(() => setCopied(true), () => setCopied(false))}
          >
            {copied ? "Copied" : "Copy"}
          </button>
        </span>
      ) : (
        <span className="flex items-center gap-2 text-[12px] text-muted">
          {state === null ? "…" : state.enabled ? "A secret is set." : "No secret."}
          <button type="button" disabled={busy || state === null} className={btn} onClick={() => act(() => previewsApi.rotateBypass(projectId))}>
            {state?.enabled ? "Rotate" : "Create a secret"}
          </button>
          {state?.enabled && (
            <button
              type="button"
              disabled={busy}
              className={btn}
              onClick={() => act(async () => (await previewsApi.deleteBypass(projectId), { enabled: false, created_at: null, header }))}
            >
              Remove
            </button>
          )}
        </span>
      )}
      {state?.secret && <span className="mt-1 block text-[11px] text-subtle">Shown only now; the previous secret stopped working.</span>}
      {error && <span role="alert" className="mt-1 block text-[11px] text-danger">{error}</span>}
    </Field>
  );
}

/** Deploy hooks (act at once, not on Save): a URL a headless CMS calls on publish, shown once when made. */
function Hooks({ projectId }: { projectId: string }) {
  const [hooks, setHooks] = useState<PreviewHook[] | null>(null);
  const [made, setMade] = useState<{ id: string; url: string } | null>(null);
  const [name, setName] = useState("");
  const [globs, setGlobs] = useState("*");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    let live = true;
    previewsApi.hooks(projectId).then((h) => live && setHooks(h.hooks), (e) => live && setError(errorText(e)));
    return () => {
      live = false;
    };
  }, [projectId]);
  const act = async (f: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const create = () =>
    act(async () => {
      const r = await previewsApi.createHook(projectId, name.trim(), words(globs));
      setHooks((h) => [...(h ?? []), r.hook]);
      setMade({ id: r.hook.id, url: new URL(r.path, ROLL_API).toString() });
      setCopied(false);
      setName("");
    });
  const btn = "h-7 rounded-md border border-line-2 px-2.5 text-[12px] text-muted transition-colors hover:text-fg disabled:opacity-50";
  return (
    <Field group label="Deploy hooks" note="A headless CMS calls one when content is published: the project's previews of those branches build again at their heads, at most once a minute.">
      {hooks?.length ? (
        <ul className="mb-2 divide-y divide-line rounded-md border border-line-2">
          {hooks.map((h) => (
            <li key={h.id} className="flex items-center gap-2 px-2.5 py-1.5 text-[12px]">
              <span className="min-w-0 truncate font-medium">{h.name}</span>
              <span className="min-w-0 truncate font-mono text-[11px] text-muted">{h.branches.join(", ")}</span>
              <span className="ml-auto shrink-0 text-[11px] text-subtle">{h.fired_at ? `ran ${new Date(h.fired_at).toLocaleString()}` : "not run yet"}</span>
              <button
                type="button"
                disabled={busy}
                className={btn}
                aria-label={`Delete the deploy hook ${h.name}`}
                onClick={() =>
                  act(async () => {
                    await previewsApi.deleteHook(projectId, h.id);
                    setHooks((all) => (all ?? []).filter((x) => x.id !== h.id));
                    if (made?.id === h.id) setMade(null);
                  })
                }
              >
                Delete
              </button>
            </li>
          ))}
        </ul>
      ) : null}
      {made && (
        <span className="mb-2 flex items-center gap-2">
          <code className={`${field} flex h-8 min-w-0 flex-1 items-center truncate font-mono text-[12px]`}>{made.url}</code>
          <button type="button" className={btn} onClick={() => navigator.clipboard?.writeText(made.url).then(() => setCopied(true), () => setCopied(false))}>
            {copied ? "Copied" : "Copy"}
          </button>
        </span>
      )}
      {made && <span className="mb-2 block text-[11px] text-subtle">Shown only now: POST to it from the CMS's publish webhook. Anyone with it can rebuild these previews.</span>}
      <span className="flex items-center gap-2">
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Name (Contentful)" aria-label="Deploy hook name" className={`${field} h-8 min-w-0 flex-1 text-[12px]`} />
        <input value={globs} onChange={(e) => setGlobs(e.target.value)} aria-label="Deploy hook branches" className={`${field} h-8 w-40 font-mono text-[12px]`} spellCheck={false} />
        <button type="button" disabled={busy || hooks === null || !name.trim() || !words(globs).length} className={btn} onClick={create}>
          Make a hook
        </button>
      </span>
      {error && <span role="alert" className="mt-1 block text-[11px] text-danger">{error}</span>}
    </Field>
  );
}

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
