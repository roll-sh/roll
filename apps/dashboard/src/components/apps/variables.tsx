import { ClipboardPaste, Copy, Eye, EyeOff, Link2, Lock, LockOpen, Plus, Rocket, Trash2, Undo2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { appsApi, describePending, parseEnv, VARIABLE_NAME, type Environment, type Link, type LinkKind, type VariableSource, type Variables } from "@/lib/apps-api";
import { KIND_LABEL, resourceKey, type ResourceView } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";
import { btn, input, panel, primary } from "../ui";
import { message, SureButton, type Runner } from "./shared";

const SOURCE_LABEL: Record<VariableSource, string> = { service: "service", secret: "secret", link: "link", roll: "roll", reference: "reference" };
const SOURCE_TONE: Record<VariableSource, string> = {
  service: "border-line-2 text-muted",
  secret: "border-line-2 text-fg",
  link: "border-accent/40 text-accent",
  roll: "border-line text-subtle",
  reference: "border-line-2 text-muted",
};

/** Workspace kinds a service can link to, by their link kind. */
/** What a service can link to today: rolld wires Postgres and Redis (buckets, streams and NoSQL come as roll secrets links, docs/apps/README.md "Next"). */
export const LINKABLE: Partial<Record<ResourceView["kind"], LinkKind>> = { postgres: "postgres", redis: "redis", service: "service" };

type Edit = { name: string; value: string; seal?: boolean };

/**
 * The environment's variables (masked; Reveal asks for the values), edited in place and saved in
 * one PATCH, then the links to the project's databases that add their own.
 */
export function VariablesTab({
  service,
  project,
  env,
  envs = [],
  runner,
  onRedeploy,
}: {
  service: string;
  project: string;
  env: Environment;
  /** The service's other environments, to copy variables from. */
  envs?: string[];
  runner: Runner;
  onRedeploy: () => void;
}) {
  const [vars, setVars] = useState<Variables | null>(null);
  const [values, setValues] = useState<Record<string, string> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [edits, setEdits] = useState<Record<string, string>>({});
  const [removed, setRemoved] = useState<Set<string>>(new Set());
  // Existing variables to seal at Save: for good, so staged until then.
  const [sealing, setSealing] = useState<Set<string>>(new Set());
  const [added, setAdded] = useState<Edit[]>([]);
  const [pasting, setPasting] = useState(false);
  const [pasted, setPasted] = useState("");
  // What the next deploy would change, named (read again whenever the environment changes).
  const [pending, setPending] = useState<string | null>(null);
  useEffect(() => {
    if (!env.needs_deploy) return setPending(null);
    let live = true;
    appsApi.pending(service, env.name).then((p) => live && setPending(describePending(p)), () => live && setPending(null));
    return () => {
      live = false;
    };
  }, [service, env.name, env.needs_deploy, env.active?.id, vars]);
  const parsed = useMemo(() => parseEnv(pasted), [pasted]);

  /** Stage `set` for review: an existing service variable becomes an edit, a new name a row. */
  const stage = (set: Record<string, string>) => {
    const have = new Set((vars?.vars ?? []).filter((v) => v.source === "service" || v.source === "reference").map((v) => v.name));
    setEdits((e) => ({ ...e, ...Object.fromEntries(Object.entries(set).filter(([k]) => have.has(k))) }));
    setAdded((a) => [...a.filter((x) => !(x.name in set)), ...Object.entries(set).filter(([k]) => !have.has(k)).map(([name, value]) => ({ name, value }))]);
  };
  const copyFrom = async (from: string) => {
    // Reading another environment's values is a reveal there: the gate decides it as one.
    const v = await runner.run("copy", () => appsApi.reveal(service, from));
    if (v) stage(v);
  };

  const load = useCallback(async () => {
    try {
      setVars(await appsApi.variables(service, env.name));
      setError(null);
    } catch (e) {
      setError(message(e));
    }
  }, [service, env.name]);
  useEffect(() => {
    setValues(null);
    setEdits({});
    setRemoved(new Set());
    setSealing(new Set());
    setAdded([]);
    void load();
  }, [load]);

  const reveal = async () => {
    if (values) return setValues(null);
    const v = await runner.run("reveal", () => appsApi.reveal(service, env.name));
    if (v) setValues(v);
  };

  const badNew = added.filter((a) => a.name && !VARIABLE_NAME.test(a.name)).length > 0;
  const toSeal = [...[...sealing].filter((n) => !removed.has(n)), ...added.filter((a) => a.name && a.seal).map((a) => a.name)];
  const dirty = Object.keys(edits).length > 0 || removed.size > 0 || added.some((a) => a.name) || sealing.size > 0;
  const discard = () => {
    setEdits({});
    setRemoved(new Set());
    setSealing(new Set());
    setAdded([]);
  };
  const save = async () => {
    const set: Record<string, string> = { ...edits };
    for (const a of added) if (a.name) set[a.name] = a.value;
    const out = await runner.run("variables", () => appsApi.setVariables(service, env.name, { set, unset: [...removed], seal: toSeal }));
    if (out) {
      setVars(out);
      discard();
      setValues(null);
    }
  };
  const toggle = (set: (f: (s: Set<string>) => Set<string>) => void, name: string) =>
    set((r) => {
      const n = new Set(r);
      if (n.has(name)) n.delete(name);
      else n.add(name);
      return n;
    });

  return (
    <div className="space-y-6">
      {env.needs_deploy && (
        <div className="rise flex flex-wrap items-center gap-3 rounded-xl border border-accent/40 bg-accent/[0.06] px-4 py-3">
          <div className="min-w-0 flex-1">
            <p className="text-[13px]">Variables or settings changed since the live deployment. Redeploy to apply them: the same image, no build.</p>
            {pending && <p className="mt-1 font-mono text-[11.5px] text-muted">{pending}</p>}
          </div>
          <button className={primary} onClick={onRedeploy} disabled={runner.busy !== null || !env.active}>
            <Rocket className="size-3.5" /> Redeploy
          </button>
        </div>
      )}

      <section className={panel}>
        <div className="flex min-h-14 flex-wrap items-center justify-between gap-3 border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-medium">Variables</h2>
          <div className="flex flex-wrap gap-2">
            {envs.length > 0 && (
              <label className={`${btn} relative`} title="Stage another environment's variables here (review, then Save)">
                <Copy className="size-3.5" /> Copy from
                <select
                  value=""
                  onChange={(e) => e.target.value && void copyFrom(e.target.value)}
                  disabled={runner.busy !== null}
                  className="absolute inset-0 cursor-pointer opacity-0"
                  aria-label="Copy variables from another environment"
                >
                  <option value="">Copy from…</option>
                  {envs.map((e) => (
                    <option key={e} value={e}>
                      {e}
                    </option>
                  ))}
                </select>
              </label>
            )}
            <button className={btn} onClick={() => setPasting((p) => !p)} aria-pressed={pasting}>
              <ClipboardPaste className="size-3.5" /> Paste .env
            </button>
            <button className={btn} onClick={reveal} disabled={runner.busy !== null || !vars?.vars.length}>
              {values ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
              {values ? "Hide" : "Reveal"}
            </button>
            <button className={btn} onClick={() => setAdded((a) => [...a, { name: "", value: "" }])}>
              <Plus className="size-3.5" /> Add
            </button>
          </div>
        </div>
        {pasting && (
          <div className="rise space-y-2 border-b border-line px-5 py-4">
            <textarea
              value={pasted}
              onChange={(e) => setPasted(e.target.value)}
              placeholder={"DATABASE_URL=postgres://…\nexport API_KEY='…'"}
              rows={5}
              spellCheck={false}
              className={`${input} w-full font-mono text-[12px]`}
              aria-label=".env text"
            />
            <div className="flex items-center justify-between gap-3">
              <span className={`text-[12px] ${parsed.errors.length ? "text-danger" : "text-subtle"}`}>
                {parsed.errors.length ? `Lines ${parsed.errors.join(", ")} aren't KEY=value.` : `${Object.keys(parsed.vars).length} variables, staged for review before Save.`}
              </span>
              <button
                className={primary}
                disabled={!Object.keys(parsed.vars).length || parsed.errors.length > 0}
                onClick={() => {
                  stage(parsed.vars);
                  setPasted("");
                  setPasting(false);
                }}
              >
                Stage
              </button>
            </div>
          </div>
        )}
        {error && <p className="px-5 py-4 text-[13px] text-danger">{error}</p>}
        {!vars && !error && <p className="px-5 py-4 text-[13px] text-subtle"><span className="shimmer">Loading…</span></p>}
        {vars && (
          <ul className="divide-y divide-line">
            {vars.vars.length === 0 && added.length === 0 && <li className="px-5 py-4 text-[13px] text-muted">No variables yet. roll sets PORT and ROLL_* on every deploy.</li>}
            {vars.vars.map((v) => {
              // A reference is its own variable: edited as its template, never as what it resolves to.
              const editable = v.source === "service" || v.source === "reference";
              const gone = removed.has(v.name);
              const editing = v.name in edits;
              return (
                <li key={v.name} className={`flex flex-wrap items-center gap-3 px-5 py-2.5 ${gone ? "opacity-50" : ""}`}>
                  <span className={`w-56 max-w-full truncate font-mono text-[12px] ${gone ? "line-through" : ""}`}>{v.name}</span>
                  <span className={`rounded border px-1.5 font-mono text-[10.5px] ${SOURCE_TONE[v.source]}`}>{SOURCE_LABEL[v.source]}</span>
                  {v.sealed && (
                    <span className="inline-flex items-center gap-1 text-[11px] text-subtle" title="Sealed: the app runs with it, nothing shows it again">
                      <Lock className="size-3" strokeWidth={1.5} /> sealed
                    </span>
                  )}
                  {editing ? (
                    <input
                      autoFocus
                      placeholder={v.sealed ? "new value (stays sealed)" : "new value"}
                      value={edits[v.name]}
                      onChange={(e) => setEdits((x) => ({ ...x, [v.name]: e.target.value }))}
                      className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
                      spellCheck={false}
                    />
                  ) : (
                    <button
                      disabled={!editable || gone}
                      onClick={() => setEdits((x) => ({ ...x, [v.name]: v.sealed ? "" : (v.reference ?? values?.[v.name] ?? "") }))}
                      className="min-w-0 flex-1 truncate text-left font-mono text-[12px] text-muted enabled:hover:text-fg"
                      title={editable ? "Edit" : `Set by ${v.source === "link" ? "a link" : v.source === "secret" ? "the project's secrets" : "roll"}`}
                    >
                      {v.sealed ? v.masked : (v.reference ?? values?.[v.name] ?? v.masked)}
                    </button>
                  )}
                  {editable && !v.sealed && !gone && (
                    <button
                      onClick={() => toggle(setSealing, v.name)}
                      className={`p-1 hover:text-fg ${sealing.has(v.name) ? "text-accent" : "text-subtle"}`}
                      aria-pressed={sealing.has(v.name)}
                      aria-label={`Seal ${v.name}`}
                      title={sealing.has(v.name) ? "Sealed at Save: click to keep it readable" : "Seal: the app runs with it, nothing shows it again"}
                    >
                      {sealing.has(v.name) ? <Lock className="size-3.5" /> : <LockOpen className="size-3.5" />}
                    </button>
                  )}
                  {editable && (
                    <button
                      onClick={() => {
                        toggle(setRemoved, v.name);
                        setEdits(({ [v.name]: _, ...rest }) => rest);
                      }}
                      className="p-1 text-subtle hover:text-fg"
                      aria-label={gone ? `Keep ${v.name}` : `Remove ${v.name}`}
                      title={gone ? "Keep" : "Remove"}
                    >
                      {gone ? <Undo2 className="size-3.5" /> : <Trash2 className="size-3.5" />}
                    </button>
                  )}
                </li>
              );
            })}
            {added.map((a, i) => (
              <li key={`new-${i}`} className="flex flex-wrap items-center gap-3 px-5 py-2.5">
                <input
                  autoFocus={i === added.length - 1}
                  value={a.name}
                  placeholder="NAME"
                  onChange={(e) => setAdded((l) => l.map((x, j) => (j === i ? { ...x, name: e.target.value.toUpperCase() } : x)))}
                  className={`${input} w-56 max-w-full font-mono text-[12px] ${a.name && !VARIABLE_NAME.test(a.name) ? "border-danger/60" : ""}`}
                  spellCheck={false}
                />
                <input
                  value={a.value}
                  placeholder="value"
                  onChange={(e) => setAdded((l) => l.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))}
                  className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
                  spellCheck={false}
                />
                <button
                  onClick={() => setAdded((l) => l.map((x, j) => (j === i ? { ...x, seal: !x.seal } : x)))}
                  className={`p-1 hover:text-fg ${a.seal ? "text-accent" : "text-subtle"}`}
                  aria-pressed={!!a.seal}
                  aria-label="Seal"
                  title={a.seal ? "Sealed at Save" : "Seal: the app runs with it, nothing shows it again"}
                >
                  {a.seal ? <Lock className="size-3.5" /> : <LockOpen className="size-3.5" />}
                </button>
                <button onClick={() => setAdded((l) => l.filter((_, j) => j !== i))} className="p-1 text-subtle hover:text-fg" aria-label="Drop">
                  <Trash2 className="size-3.5" />
                </button>
              </li>
            ))}
          </ul>
        )}
        {dirty && (
          <div className="flex items-center justify-end gap-2 border-t border-line px-5 py-3">
            {toSeal.length > 0 && <span className="mr-auto text-[12px] text-subtle">Sealing {toSeal.join(", ")} is for good: nothing shows {toSeal.length === 1 ? "it" : "them"} again.</span>}
            <button className={btn} onClick={discard}>
              Discard
            </button>
            <button className={primary} disabled={runner.busy !== null || badNew} onClick={save}>
              {runner.busy === "variables" ? <span className="shimmer">Saving…</span> : "Save"}
            </button>
          </div>
        )}
      </section>

      <Links service={service} project={project} runner={runner} onChange={load} />
    </div>
  );
}

function Links({ service, project, runner, onChange }: { service: string; project: string; runner: Runner; onChange: () => void }) {
  const ws = useWorkspace();
  const [links, setLinks] = useState<Link[] | null>(null);
  const [pick, setPick] = useState("");
  const [prefix, setPrefix] = useState("");

  const load = useCallback(() => appsApi.links(service).then(setLinks, () => setLinks([])), [service]);
  useEffect(() => {
    void load();
  }, [load]);

  // The project's databases and its other services, by `kind:ref`.
  const candidates = useMemo(() => {
    const p = ws.projects.find((x) => x.id === project);
    return (p?.resources ?? [])
      .filter((r) => LINKABLE[r.kind] && !(r.kind === "service" && r.ref === service))
      .map((r) => ws.inventory.get(resourceKey(r)))
      .filter((v): v is ResourceView => !!v && v.state !== "missing");
  }, [ws.projects, ws.inventory, project, service]);
  const linked = new Set((links ?? []).map((l) => `${l.kind}:${l.ref}`));
  const nameOf = (l: Link) => ws.inventory.get(`${l.kind}:${l.ref}`)?.name ?? l.ref;

  const add = async () => {
    const v = candidates.find((c) => resourceKey(c) === pick);
    const kind = v && LINKABLE[v.kind];
    if (!v || !kind) return;
    const out = await runner.run("link", () => appsApi.link(service, kind, v.ref, prefix.trim() || undefined));
    if (out) {
      setPick("");
      setPrefix("");
      await load();
      onChange();
    }
  };

  return (
    <section className={panel}>
      <div className="flex min-h-14 items-center justify-between gap-3 border-b border-line px-5 py-3">
        <h2 className="text-[15px] font-medium">Links</h2>
        <span className="text-[12px] text-subtle">A link adds a database's connection variables, or another service's URLs.</span>
      </div>
      <ul className="divide-y divide-line">
        {links?.length === 0 && <li className="px-5 py-4 text-[13px] text-muted">Nothing linked.</li>}
        {links?.map((l) => (
          <li key={`${l.kind}:${l.ref}`} className="flex flex-wrap items-center gap-3 px-5 py-3">
            <Link2 className="size-3.5 text-subtle" />
            <span className="text-[13px] font-medium">{nameOf(l)}</span>
            <span className="text-[12px] text-subtle">{l.kind}</span>
            {l.prefix && <span className="rounded border border-line-2 px-1.5 font-mono text-[10.5px] text-muted">{l.prefix}_</span>}
            <span className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-muted">{l.vars.join(" ")}</span>
            <SureButton
              className="p-1 text-subtle hover:text-fg"
              label="Unlink"
              title="Unlink: its variables leave the next deploy"
              sure="Sure? Unlink"
              disabled={runner.busy !== null}
              onSure={async () => {
                if (await runner.run(`unlink:${l.ref}`, () => appsApi.unlink(service, l.kind, l.ref).then(() => true))) {
                  await load();
                  onChange();
                }
              }}
            >
              <Trash2 className="size-3.5" />
            </SureButton>
          </li>
        ))}
      </ul>
      <div className="flex flex-wrap items-center gap-2 border-t border-line px-5 py-3">
        <select value={pick} onChange={(e) => setPick(e.target.value)} className={`${input} min-w-0 flex-1`} aria-label="Resource to link">
          <option value="">{candidates.length ? "Link a resource in this project…" : "Nothing else in this project to link yet"}</option>
          {candidates.map((c) => (
            <option key={resourceKey(c)} value={resourceKey(c)} disabled={linked.has(resourceKey(c))}>
              {KIND_LABEL[c.kind]} · {c.name}
            </option>
          ))}
        </select>
        <input value={prefix} onChange={(e) => setPrefix(e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, ""))} placeholder="PREFIX (optional)" className={`${input} w-44 font-mono text-[12px]`} />
        <button className={primary} disabled={!pick || runner.busy !== null} onClick={add}>
          Link
        </button>
      </div>
    </section>
  );
}
