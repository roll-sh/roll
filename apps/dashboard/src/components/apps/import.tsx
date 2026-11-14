import { FileUp } from "lucide-react";
import { useRef, useState } from "react";
import { appsApi, type Link, type ProjectImport, type Service } from "@/lib/apps-api";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { freeSpot, resourceKey, service as serviceView } from "@/lib/workspace-resources";
import { PlaceError, refreshInventory, useWorkspace, workspace } from "@/lib/workspace-store";
import { Label } from "../workspace/inspector";

type Db = "postgres" | "redis";

/** One line of what an import does, as `roll up --all` prints it: `+` made, `=` there, `~`
 *  changed, `↑` deployed, `!` stops it. */
export type ImportStep =
  | { op: "make-db"; kind: Db; name: string }
  | { op: "have-db"; kind: Db; name: string }
  | { op: "make-service"; name: string; from: string; upload: boolean }
  | { op: "have-service"; name: string; from: string }
  | { op: "link"; service: string; db: string }
  | { op: "settings"; service: string; keys: string[] }
  | { op: "deploy"; service: string }
  | { op: "conflict"; service: string; text: string };

/** What's there: the project's services by name (with their links), its databases by kind and name. */
export interface ImportHave {
  services: Map<string, { service: Service; links: Link[] }>;
  dbs: Map<string, string>;
}

const dbKey = (kind: string, name: string) => `${kind}:${name}`;
const an = (kind: string) => `${kind === "image" || kind === "upload" ? "an" : "a"} ${kind}`;

/** Equal as JSON values, whatever their keys' order. */
function same(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  return ka.length === kb.length && ka.every((k) => same((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k]));
}

function fromLabel(source: ProjectImport["apps"][string]["source"]) {
  if (source.kind === "repo") return `repo ${source.repo}${source.root ? ` at ${source.root}` : ""}`;
  if (source.kind === "image") return `image ${source.image}`;
  return `from ${source.root}`;
}

/** What importing `imp` into a project that has `have` does, in order: databases, then each
 *  service with its links, settings and deploy. Names are matched, so importing again only does
 *  what changed; nothing is ever deleted. A service of another kind of source stops it. */
export function planImport(imp: ProjectImport, have: ImportHave): ImportStep[] {
  const out: ImportStep[] = [];
  for (const [name, d] of Object.entries(imp.databases)) out.push({ op: have.dbs.has(dbKey(d.kind, name)) ? "have-db" : "make-db", kind: d.kind, name });
  for (const [name, a] of Object.entries(imp.apps)) {
    const there = have.services.get(name);
    const from = fromLabel(a.source);
    if (!there) {
      out.push({ op: "make-service", name, from, upload: a.source.kind === "upload" });
      for (const l of a.links) out.push({ op: "link", service: name, db: l.name });
      continue;
    }
    if (there.service.source.kind !== a.source.kind) {
      out.push({ op: "conflict", service: name, text: `${name} has ${an(there.service.source.kind)} source; the file makes it ${an(a.source.kind)} one: rename one of them` });
      continue;
    }
    out.push({ op: "have-service", name, from });
    for (const l of a.links) {
      const ref = have.dbs.get(dbKey(l.kind, l.name));
      if (!ref || !there.links.some((x) => x.kind === l.kind && x.ref === ref)) out.push({ op: "link", service: name, db: l.name });
    }
    const run = there.service.run as unknown as Record<string, unknown>;
    const keys = Object.entries(a.run).filter(([k, v]) => !same(run[k], v)).map(([k]) => k);
    if (keys.length) out.push({ op: "settings", service: name, keys });
    if (a.source.kind === "image" && there.service.source.kind === "image" && there.service.source.image !== a.source.image) out.push({ op: "deploy", service: name });
  }
  return out;
}

export function stepLine(s: ImportStep): { mark: string; text: string } {
  switch (s.op) {
    case "make-db":
      return { mark: "+", text: `${s.kind} database ${s.name}` };
    case "have-db":
      return { mark: "=", text: `${s.kind} database ${s.name}` };
    case "make-service":
      return { mark: "+", text: `service ${s.name} (${s.from})${s.upload ? ": `roll up --all` uploads its code" : ""}` };
    case "have-service":
      return { mark: "=", text: `service ${s.name} (${s.from})` };
    case "link":
      return { mark: "~", text: `link ${s.service} to ${s.db}` };
    case "settings":
      return { mark: "~", text: `settings ${s.service} (${s.keys.join(", ")})` };
    case "deploy":
      return { mark: "↑", text: `deploy ${s.service}` };
    case "conflict":
      return { mark: "!", text: s.text };
  }
}

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** A roll.toml (with `[apps]`) or a Compose file, read by rolld; nothing about it decides here. */
function fileOf(text: string, filename: string): { roll_toml: string } | { compose: string } {
  if (/\.toml$/i.test(filename) || (!/\.ya?ml$/i.test(filename) && /^\s*\[(apps|databases)\./m.test(text))) return { roll_toml: text };
  return { compose: text };
}

/**
 * New service's "From a file": a project's roll.toml or a Compose file, read by rolld, planned
 * against what the project has (as `roll up --all` plans it), and applied with the usual calls:
 * databases made and placed first, so their variables are in the services' first deploys.
 */
export function ImportPane({ project, spot, onDone }: { project: WorkspaceProject; spot: { x: number; y: number }; onDone: (made: Service[]) => void }) {
  const ws = useWorkspace();
  const [text, setText] = useState("");
  const [filename, setFilename] = useState("");
  const [read, setRead] = useState<{ imp: ProjectImport; have: ImportHave; steps: ImportStep[] } | null>(null);
  const [busy, setBusy] = useState<"" | "reading" | "applying">("");
  const [step, setStep] = useState("");
  const [error, setError] = useState<string | null>(null);
  const picker = useRef<HTMLInputElement>(null);

  const look = async () => {
    setBusy("reading");
    setError(null);
    try {
      const imp = await appsApi.importProject(fileOf(text, filename));
      const services = new Map<string, { service: Service; links: Link[] }>();
      for (const s of await appsApi.services(project.id)) services.set(s.name, { service: s, links: await appsApi.links(s.id).catch(() => []) });
      // Its databases are the ones on its canvas: another project's `db` is never this one's.
      const dbs = new Map<string, string>();
      for (const r of project.resources) {
        if (r.kind !== "postgres" && r.kind !== "redis") continue;
        const v = ws.inventory.get(resourceKey(r));
        if (v) dbs.set(dbKey(r.kind, v.name), r.ref);
      }
      const have = { services, dbs };
      setRead({ imp, have, steps: planImport(imp, have) });
    } catch (e) {
      setRead(null);
      setError(message(e));
    } finally {
      setBusy("");
    }
  };

  const apply = async () => {
    if (!read) return;
    const { imp, have } = read;
    setBusy("applying");
    setError(null);
    const made: Service[] = [];
    try {
      const taken: { x: number; y: number }[] = [...project.resources, spot];
      const dbs = new Map(have.dbs);
      for (const [name, d] of Object.entries(imp.databases)) {
        if (dbs.has(dbKey(d.kind, name))) continue;
        setStep(`Making the ${d.kind} database ${name}…`);
        const at = freeSpot(taken);
        taken.push(at);
        const ref = await workspace.make(project.id, d.kind, name, at).catch((e: unknown) => {
          if (e instanceof PlaceError) return e.ref;
          throw e;
        });
        dbs.set(dbKey(d.kind, name), ref);
      }
      for (const [name, a] of Object.entries(imp.apps)) {
        const links = a.links.map((l) => ({ kind: l.kind, ref: dbs.get(dbKey(l.kind, l.name))! }));
        const there = have.services.get(name);
        if (!there) {
          setStep(`Making ${name}…`);
          const at = taken.length === project.resources.length + 1 ? spot : freeSpot(taken);
          taken.push(at);
          const source = a.source.kind === "repo" ? { kind: "repo" as const, repo: a.source.repo, ...(a.source.root ? { root: a.source.root } : {}) } : a.source.kind === "image" ? { kind: "image" as const, image: a.source.image } : { kind: "upload" as const };
          const s = await appsApi.create({ project: project.id, name, source, ...(Object.keys(a.run).length ? { run: a.run } : {}), ...(Object.keys(a.variables).length ? { variables: a.variables } : {}), ...(links.length ? { links } : {}), ...at });
          workspace.added(project.id, serviceView(s), at.x, at.y);
          if (a.branch) await appsApi.putEnvironment(s.id, "production", { branch: a.branch });
          made.push(s);
          continue;
        }
        const id = there.service.id;
        const run = there.service.run as unknown as Record<string, unknown>;
        const differs = Object.fromEntries(Object.entries(a.run).filter(([k, v]) => !same(run[k], v)));
        if (Object.keys(differs).length) {
          setStep(`Setting ${name}…`);
          await appsApi.update(id, { run: differs });
        }
        for (const l of links) if (!there.links.some((x) => x.kind === l.kind && x.ref === l.ref)) await appsApi.link(id, l.kind, l.ref);
        if (a.source.kind === "image" && there.service.source.kind === "image" && there.service.source.image !== a.source.image) {
          setStep(`Deploying ${name}…`);
          await appsApi.update(id, { source: { kind: "image", image: a.source.image } });
          await appsApi.deploy(id, "production");
        }
      }
      void refreshInventory();
      onDone(made);
    } catch (e) {
      // What's made stays; importing again does only what's left.
      setError(message(e));
      setRead(null);
      void refreshInventory();
    } finally {
      setBusy("");
      setStep("");
    }
  };

  const stops = read?.steps.some((s) => s.op === "conflict") ?? false;
  const todo = read?.steps.filter((s) => s.op !== "have-db" && s.op !== "have-service").length ?? 0;

  return (
    <>
      <Label>
        roll.toml or Compose file <span className="font-normal">read by rolld; nothing is made until you apply</span>
      </Label>
      <textarea
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          setRead(null);
        }}
        placeholder={'[apps.web]\nimage = "ghcr.io/acme/web:1"\nlinks = ["db"]\n[databases.db]\nkind = "postgres"'}
        spellCheck={false}
        rows={9}
        aria-label="roll.toml or Compose file"
        className="w-full resize-y rounded-lg border border-line-2 bg-canvas px-2.5 py-2 font-mono text-[12px] outline-none placeholder:text-subtle focus:border-accent/60"
      />
      <div className="mt-2 flex items-center gap-2">
        <input
          ref={picker}
          type="file"
          accept=".toml,.yaml,.yml"
          className="hidden"
          onChange={async (e) => {
            const f = e.target.files?.[0];
            if (!f) return;
            setFilename(f.name);
            setText(await f.text());
            setRead(null);
            e.target.value = "";
          }}
        />
        <button type="button" onClick={() => picker.current?.click()} className="inline-flex h-7 items-center gap-1.5 rounded-md border border-line-2 px-2 text-[12px] text-muted hover:text-fg">
          <FileUp className="size-3.5" /> Open a file
        </button>
        {filename && <span className="truncate font-mono text-[11px] text-subtle">{filename}</span>}
        <button type="button" disabled={!text.trim() || busy !== ""} onClick={() => void look()} className="ml-auto inline-flex h-7 items-center rounded-md bg-fg/[0.07] px-2.5 text-[12px] text-fg hover:bg-fg/[0.1] disabled:opacity-50">
          {busy === "reading" ? <span className="shimmer">Reading…</span> : "Read it"}
        </button>
      </div>
      {error && <p className="mt-2 text-[11.5px] text-danger">{error}</p>}
      {read && (
        <div className="rise mt-3">
          <Label>What it does</Label>
          <ul className="space-y-0.5 font-mono text-[11.5px]">
            {read.steps.map((s, i) => {
              const l = stepLine(s);
              return (
                <li key={i} className={`flex gap-2 ${s.op === "conflict" ? "text-danger" : l.mark === "=" ? "text-subtle" : "text-fg"}`}>
                  <span className={`w-3 shrink-0 ${l.mark === "+" || l.mark === "↑" ? "text-accent" : ""}`}>{l.mark}</span>
                  <span className="min-w-0 break-words">{l.text}</span>
                </li>
              );
            })}
          </ul>
          {read.imp.notes.length > 0 && (
            <ul className="mt-2 space-y-0.5 text-[11px] leading-relaxed text-muted">
              {read.imp.notes.map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
          )}
          {"compose" in fileOf(text, filename) && (
            <a
              href={`data:text/plain;charset=utf-8,${encodeURIComponent(read.imp.roll_toml)}`}
              download="roll.toml"
              className="mt-2 inline-block text-[11.5px] text-accent hover:underline"
            >
              Save it as roll.toml
            </a>
          )}
          <div className="mt-3 flex items-center gap-2">
            {step ? <span className="shimmer text-[11.5px] text-muted">{step}</span> : <span className="text-[11px] text-subtle">{stops ? "Rename the service in the file, or in roll, then read it again." : todo ? "Nothing is ever deleted: what the file leaves out stays." : "Everything in it is already here."}</span>}
            <button type="button" disabled={stops || !todo || busy !== ""} onClick={() => void apply()} className="ml-auto inline-flex h-7 items-center rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg disabled:opacity-50">
              {busy === "applying" ? "Applying…" : "Apply"}
            </button>
          </div>
        </div>
      )}
    </>
  );
}
