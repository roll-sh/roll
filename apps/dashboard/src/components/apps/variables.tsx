import { Eye, EyeOff, Link2, Plus, Rocket, Trash2, Undo2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { appsApi, VARIABLE_NAME, type Environment, type Link, type LinkKind, type VariableSource, type Variables } from "@/lib/apps-api";
import { KIND_LABEL, resourceKey, type ResourceView } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";
import { btn, input, panel, primary } from "../ui";
import { message, type Runner } from "./shared";

const SOURCE_LABEL: Record<VariableSource, string> = { service: "service", secret: "secret", link: "link", roll: "roll" };
const SOURCE_TONE: Record<VariableSource, string> = {
  service: "border-line-2 text-muted",
  secret: "border-line-2 text-fg",
  link: "border-accent/40 text-accent",
  roll: "border-line text-subtle",
};

/** Workspace kinds a service can link to, by their link kind. */
const LINKABLE: Partial<Record<ResourceView["kind"], LinkKind>> = { postgres: "postgres", redis: "redis", bucket: "bucket", stream: "stream", nosql: "nosql" };

type Edit = { name: string; value: string };

/**
 * The environment's variables (masked; Reveal asks for the values), edited in place and saved in
 * one PATCH, then the links to the project's databases that add their own.
 */
export function VariablesTab({ service, project, env, runner, onRedeploy }: { service: string; project: string; env: Environment; runner: Runner; onRedeploy: () => void }) {
  const [vars, setVars] = useState<Variables | null>(null);
  const [values, setValues] = useState<Record<string, string> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [edits, setEdits] = useState<Record<string, string>>({});
  const [removed, setRemoved] = useState<Set<string>>(new Set());
  const [added, setAdded] = useState<Edit[]>([]);

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
    setAdded([]);
    void load();
  }, [load]);

  const reveal = async () => {
    if (values) return setValues(null);
    const v = await runner.run("reveal", () => appsApi.reveal(service, env.name));
    if (v) setValues(v);
  };

  const badNew = added.filter((a) => a.name && !VARIABLE_NAME.test(a.name)).length > 0;
  const dirty = Object.keys(edits).length > 0 || removed.size > 0 || added.some((a) => a.name);
  const save = async () => {
    const set: Record<string, string> = { ...edits };
    for (const a of added) if (a.name) set[a.name] = a.value;
    const out = await runner.run("variables", () => appsApi.setVariables(service, env.name, { set, unset: [...removed] }));
    if (out) {
      setVars(out);
      setEdits({});
      setRemoved(new Set());
      setAdded([]);
      setValues(null);
    }
  };

  return (
    <div className="space-y-6">
      {env.needs_deploy && (
        <div className="rise flex flex-wrap items-center gap-3 rounded-xl border border-accent/40 bg-accent/[0.06] px-4 py-3">
          <p className="min-w-0 flex-1 text-[13px]">Variables or settings changed since the live deployment. Redeploy to apply them: the same image, no build.</p>
          <button className={primary} onClick={onRedeploy} disabled={runner.busy !== null || !env.active}>
            <Rocket className="size-3.5" /> Redeploy
          </button>
        </div>
      )}

      <section className={panel}>
        <div className="flex min-h-14 flex-wrap items-center justify-between gap-3 border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-medium">Variables</h2>
          <div className="flex gap-2">
            <button className={btn} onClick={reveal} disabled={runner.busy !== null || !vars?.vars.length}>
              {values ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
              {values ? "Hide" : "Reveal"}
            </button>
            <button className={btn} onClick={() => setAdded((a) => [...a, { name: "", value: "" }])}>
              <Plus className="size-3.5" /> Add
            </button>
          </div>
        </div>
        {error && <p className="px-5 py-4 text-[13px] text-danger">{error}</p>}
        {!vars && !error && <p className="px-5 py-4 text-[13px] text-subtle"><span className="shimmer">Loading…</span></p>}
        {vars && (
          <ul className="divide-y divide-line">
            {vars.vars.length === 0 && added.length === 0 && <li className="px-5 py-4 text-[13px] text-muted">No variables yet. roll sets PORT and ROLL_* on every deploy.</li>}
            {vars.vars.map((v) => {
              const editable = v.source === "service";
              const gone = removed.has(v.name);
              const editing = v.name in edits;
              return (
                <li key={v.name} className={`flex flex-wrap items-center gap-3 px-5 py-2.5 ${gone ? "opacity-50" : ""}`}>
                  <span className={`w-56 max-w-full truncate font-mono text-[12px] ${gone ? "line-through" : ""}`}>{v.name}</span>
                  <span className={`rounded border px-1.5 font-mono text-[10.5px] ${SOURCE_TONE[v.source]}`}>{SOURCE_LABEL[v.source]}</span>
                  {editing ? (
                    <input
                      autoFocus
                      placeholder="new value"
                      value={edits[v.name]}
                      onChange={(e) => setEdits((x) => ({ ...x, [v.name]: e.target.value }))}
                      className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
                      spellCheck={false}
                    />
                  ) : (
                    <button
                      disabled={!editable || gone}
                      onClick={() => setEdits((x) => ({ ...x, [v.name]: values?.[v.name] ?? "" }))}
                      className="min-w-0 flex-1 truncate text-left font-mono text-[12px] text-muted enabled:hover:text-fg"
                      title={editable ? "Edit" : `Set by ${v.source === "link" ? "a link" : v.source === "secret" ? "the project's secrets" : "roll"}`}
                    >
                      {values?.[v.name] ?? v.masked}
                    </button>
                  )}
                  {editable && (
                    <button
                      onClick={() => {
                        setRemoved((r) => {
                          const n = new Set(r);
                          if (n.has(v.name)) n.delete(v.name);
                          else n.add(v.name);
                          return n;
                        });
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
                <button onClick={() => setAdded((l) => l.filter((_, j) => j !== i))} className="p-1 text-subtle hover:text-fg" aria-label="Drop">
                  <Trash2 className="size-3.5" />
                </button>
              </li>
            ))}
          </ul>
        )}
        {dirty && (
          <div className="flex items-center justify-end gap-2 border-t border-line px-5 py-3">
            <button
              className={btn}
              onClick={() => {
                setEdits({});
                setRemoved(new Set());
                setAdded([]);
              }}
            >
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

  // The project's databases and storage, by `kind:ref`.
  const candidates = useMemo(() => {
    const p = ws.projects.find((x) => x.id === project);
    return (p?.resources ?? [])
      .filter((r) => LINKABLE[r.kind])
      .map((r) => ws.inventory.get(resourceKey(r)))
      .filter((v): v is ResourceView => !!v && v.state !== "missing");
  }, [ws.projects, ws.inventory, project]);
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
        <span className="text-[12px] text-subtle">A link adds the resource's connection variables.</span>
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
            <button
              className="p-1 text-subtle hover:text-fg"
              aria-label="Unlink"
              title="Unlink"
              disabled={runner.busy !== null}
              onClick={async () => {
                if (await runner.run(`unlink:${l.ref}`, () => appsApi.unlink(service, l.kind, l.ref).then(() => true))) {
                  await load();
                  onChange();
                }
              }}
            >
              <Trash2 className="size-3.5" />
            </button>
          </li>
        ))}
      </ul>
      <div className="flex flex-wrap items-center gap-2 border-t border-line px-5 py-3">
        <select value={pick} onChange={(e) => setPick(e.target.value)} className={`${input} min-w-0 flex-1`} aria-label="Resource to link">
          <option value="">{candidates.length ? "Link a resource in this project…" : "No databases or storage in this project yet"}</option>
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
