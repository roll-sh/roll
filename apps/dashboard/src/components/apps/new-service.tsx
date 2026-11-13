import { ChevronRight, GitBranch, Image, LoaderCircle, Terminal } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { appsApi, parseEnv, SERVICE_NAME, type Plan, type Service, type ServiceType, type Source } from "@/lib/apps-api";
import { codeApi, type GithubInstallation, type Owner, type Repo } from "@/lib/code-api";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { CopyField, Head, Label, Rows } from "../workspace/inspector";
import { KindTile } from "../workspace/kind";

type From = "repo" | "image" | "upload";

const FROM: { value: From; label: string; Icon: typeof GitBranch }[] = [
  { value: "repo", label: "Repository", Icon: GitBranch },
  { value: "image", label: "Image", Icon: Image },
  { value: "upload", label: "Your machine", Icon: Terminal },
];

const field = "h-8 w-full rounded-lg border border-line-2 bg-canvas px-2.5 font-mono text-[12px] outline-none placeholder:text-subtle focus:border-accent/60";

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** A word for `sh`: as is when it's plain, else in single quotes. */
const shellQuote = (w: string) => (/^[A-Za-z0-9_./-]+$/.test(w) ? w : `'${w.replace(/'/g, `'\\''`)}'`);

/** A name rolld takes, from a repo or image name: `acme/web-site` → `web-site`, `nginx:1.27` → `nginx`. */
export function nameFrom(s: string) {
  const last = s.split("/").pop()?.split(/[:@]/)[0] ?? "";
  return last.toLowerCase().replace(/[^a-z0-9-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 32).replace(/-+$/, "");
}

/**
 * New service, in the canvas's sheet: a roll code repo (with the build plan rolld detects shown
 * before anything is made, so the build is never magic), a public image, or `roll up` from a
 * machine. A repo or an image deploys to production as soon as the service is made.
 */
export function NewServiceSheet({
  project,
  spot,
  onClose,
  onCreated,
}: {
  project: WorkspaceProject;
  spot: { x: number; y: number };
  onClose: () => void;
  onCreated: (s: Service) => void;
}) {
  const [from, setFrom] = useState<From>("repo");
  const [repos, setRepos] = useState<Repo[] | null>(null);
  const [reposError, setReposError] = useState<string | null>(null);
  const [repo, setRepo] = useState("");
  const [root, setRoot] = useState("");
  const [image, setImage] = useState("");
  const [regUser, setRegUser] = useState("");
  const [regPassword, setRegPassword] = useState("");
  const [name, setName] = useState("");
  const [named, setNamed] = useState(false);
  const [port, setPort] = useState("");
  const [kind, setKind] = useState<ServiceType>("web");
  const [schedule, setSchedule] = useState("");
  const [env, setEnv] = useState("");
  const [plan, setPlan] = useState<{ key: string; plan?: Plan; error?: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // The project's own repositories first.
  const inProject = useMemo(() => new Set(project.resources.filter((r) => r.kind === "repo").map((r) => r.ref)), [project.resources]);
  const [reload, setReload] = useState(0);
  useEffect(() => {
    codeApi.repos().then(
      (l) => {
        const sorted = [...l].sort((a, b) => Number(inProject.has(b.full_name)) - Number(inProject.has(a.full_name)) || a.full_name.localeCompare(b.full_name));
        setRepos(sorted);
        setRepo((r) => r || sorted[0]?.full_name || "");
      },
      (e) => setReposError(message(e)),
    );
  }, [inProject, reload]);

  // The name follows the source until it's typed.
  useEffect(() => {
    if (named) return;
    const base = from === "repo" ? nameFrom(repo) : from === "image" ? nameFrom(image) : "";
    setName(base);
  }, [from, repo, image, named]);

  // rolld's plan for the repo and root, looked up when they settle.
  const planKey = `${repo}\n${root.trim()}`;
  useEffect(() => {
    if (from !== "repo" || !repo) return;
    let live = true;
    const t = setTimeout(() => {
      setPlan({ key: planKey });
      appsApi.plan({ repo, ...(root.trim() ? { root: root.trim() } : {}) }).then(
        (p) => live && setPlan({ key: planKey, plan: p }),
        (e) => live && setPlan({ key: planKey, error: message(e) }),
      );
    }, 350);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [from, repo, root, planKey]);

  const source: Source | null =
    from === "repo" ? (repo ? { kind: "repo", repo, ...(root.trim() ? { root: root.trim() } : {}) } : null) : from === "image" ? (image.trim() ? { kind: "image", image: image.trim() } : null) : null;
  const portN = port.trim() ? Number(port) : undefined;
  const portOk = portN === undefined || (Number.isInteger(portN) && portN > 0 && portN < 65536);
  const parsed = useMemo(() => parseEnv(env), [env]);
  const scheduleOk = kind !== "cron" || schedule.trim().split(/\s+/).length === 5;
  const ready = !!source && SERVICE_NAME.test(name) && portOk && scheduleOk && parsed.errors.length === 0 && !busy;

  const create = async () => {
    if (!source) return;
    setBusy(true);
    setError(null);
    try {
      const variables = Object.keys(parsed.vars).length ? { variables: parsed.vars } : {};
      const run = kind === "web" ? (portN ? { port: portN } : undefined) : kind === "cron" ? { type: kind, schedule: schedule.trim() } : { type: kind };
      const registry = from === "image" && regUser.trim() && regPassword ? { registry: { username: regUser.trim(), password: regPassword } } : {};
      const s = await appsApi.create({ project: project.id, name, source, ...(run ? { run } : {}), ...variables, ...registry, ...spot });
      onCreated(s);
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Head tile={<KindTile kind="service" size={30} />} title="New service" sub="Your code, scales to zero" onClose={onClose} />
      <div className="flex gap-1 border-b border-line px-3 pb-2" role="radiogroup" aria-label="Source">
        {FROM.map(({ value, label, Icon }) => (
          <button
            key={value}
            role="radio"
            aria-checked={from === value}
            onClick={() => setFrom(value)}
            className={`flex h-7 flex-1 items-center justify-center gap-1.5 rounded-md text-[12px] transition-colors ${
              from === value ? "bg-fg/[0.07] text-fg" : "text-muted hover:bg-fg/[0.04] hover:text-fg"
            }`}
          >
            <Icon className="size-3.5" />
            {label}
          </button>
        ))}
      </div>
      <div key={from} className="rise flex-1 overflow-y-auto p-3 [scrollbar-width:none]">
        {from === "repo" && (
          <>
            <Label>Repository</Label>
            {reposError ? (
              <p className="text-[11.5px] text-danger">roll code didn't answer: {reposError}</p>
            ) : repos === null ? (
              <p className="text-[11.5px] text-subtle">
                <span className="shimmer">Loading repositories…</span>
              </p>
            ) : repos.length === 0 ? (
              <p className="text-[11.5px] leading-relaxed text-muted">There are no repositories in roll code yet: push one, or bring one from GitHub below.</p>
            ) : (
              <select value={repo} onChange={(e) => setRepo(e.target.value)} className={field}>
                {repos.map((r) => (
                  <option key={r.id} value={r.full_name}>
                    {r.full_name}
                    {inProject.has(r.full_name) ? " · in this project" : ""}
                  </option>
                ))}
              </select>
            )}
            <FromGithub
              onPicked={(full) => {
                setReload((n) => n + 1);
                setRepo(full);
              }}
            />
            <Label>
              Root directory <span className="font-normal">optional</span>
            </Label>
            <input value={root} onChange={(e) => setRoot(e.target.value)} placeholder="apps/web" className={field} spellCheck={false} />
          </>
        )}
        {from === "image" && (
          <>
            <Label>Image</Label>
            <input value={image} onChange={(e) => setImage(e.target.value)} placeholder="docker.io/library/nginx:1.27" className={field} spellCheck={false} autoFocus />
            <p className="mt-1.5 text-[11px] leading-relaxed text-subtle">Each deploy pins its digest, so a moving tag never changes what's running.</p>
            <Label>Private registry login (optional)</Label>
            <div className="flex gap-2">
              <input value={regUser} onChange={(e) => setRegUser(e.target.value)} placeholder="username" className={field} spellCheck={false} autoComplete="off" />
              <input value={regPassword} onChange={(e) => setRegPassword(e.target.value)} placeholder="password or token" type="password" className={field} autoComplete="new-password" />
            </div>
            <p className="mt-1.5 text-[11px] leading-relaxed text-subtle">Sealed, sent only to the image's own registry, never shown again.</p>
          </>
        )}
        {from === "upload" ? (
          <>
            <Label>From the app's directory</Label>
            <CopyField shown={`roll up --project ${shellQuote(project.name)}`} value={`roll up --project ${shellQuote(project.name)}`} />
            <p className="mt-2 text-[11.5px] leading-relaxed text-muted">
              Makes a service named after the directory in {project.name}, uploads the tree and follows the build. Its card shows up here; run it again to deploy what changed.
            </p>
          </>
        ) : (
          <>
            <Label>Name</Label>
            <input
              value={name}
              onChange={(e) => {
                setNamed(true);
                setName(e.target.value.toLowerCase());
              }}
              className={`${field} ${name && !SERVICE_NAME.test(name) ? "border-danger/60" : ""}`}
              spellCheck={false}
            />
            <Label>Type</Label>
            <div role="radiogroup" aria-label="Type" className="grid grid-cols-3 gap-1 rounded-lg border border-line p-0.5">
              {(["web", "worker", "cron"] as const).map((k) => (
                <button
                  key={k}
                  type="button"
                  role="radio"
                  aria-checked={kind === k}
                  onClick={() => setKind(k)}
                  className={`rounded-md px-2 py-1 text-[11.5px] transition-colors duration-150 ${kind === k ? "bg-white/[0.08] text-fg" : "text-muted hover:text-fg"}`}
                >
                  {k === "web" ? "Web" : k === "worker" ? "Worker" : "Cron"}
                </button>
              ))}
            </div>
            <p className="mt-1.5 text-[11px] leading-relaxed text-subtle">
              {kind === "web" ? "Answers HTTP; sleeps when idle and wakes on the first request." : kind === "worker" ? "Runs all the time, with no public URL." : "Runs on a schedule, to its exit."}
            </p>
            {kind === "web" && (
              <>
                <Label>
                  Port <span className="font-normal">{from === "repo" && plan?.plan ? `detected ${plan.plan.port}` : "default 8080"}</span>
                </Label>
                <input value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} placeholder={String(plan?.plan?.port ?? 8080)} inputMode="numeric" className={field} />
              </>
            )}
            {kind === "cron" && (
              <>
                <Label>
                  Schedule <span className="font-normal">five fields, UTC</span>
                </Label>
                <input value={schedule} onChange={(e) => setSchedule(e.target.value)} placeholder="0 3 * * *" spellCheck={false} className={`${field} font-mono ${schedule && !scheduleOk ? "border-danger/60" : ""}`} />
              </>
            )}
            <Label>
              Variables <span className="font-normal">optional: KEY=value lines, a .env pasted as is</span>
            </Label>
            <textarea
              value={env}
              onChange={(e) => setEnv(e.target.value)}
              placeholder={"DATABASE_URL=postgres://…\nNODE_ENV=production"}
              rows={3}
              spellCheck={false}
              aria-invalid={parsed.errors.length > 0}
              className={`${field} h-auto resize-y py-1.5 font-mono ${parsed.errors.length ? "border-danger/60" : ""}`}
            />
            {parsed.errors.length > 0 && <p className="mt-1 text-[11px] text-danger">Line {parsed.errors.join(", ")} isn't KEY=value.</p>}
            {from === "repo" && <PlanView state={!repo ? null : plan?.key === planKey ? plan : {}} />}
          </>
        )}
        {error && <p className="mt-3 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-2 text-[11.5px] text-danger">{error}</p>}
      </div>
      <div className="flex items-center gap-2 border-t border-line px-3 py-2">
        <span className="mr-auto text-[11px] text-subtle">{from === "upload" ? "Nothing is made until you run it." : "Deploys to production when it's made."}</span>
        <button onClick={onClose} className="rounded-md px-2 py-1 text-[11.5px] text-muted hover:bg-white/[0.05] hover:text-fg">
          {from === "upload" ? "Done" : "Cancel"}
        </button>
        {from !== "upload" && (
          <button
            onClick={create}
            disabled={!ready}
            className="flex h-7 items-center gap-1.5 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-50"
          >
            {busy && <LoaderCircle className="size-3 animate-spin motion-reduce:animate-none" />}
            {busy ? "Deploying…" : "Deploy"}
          </button>
        )}
      </div>
    </>
  );
}

/** The plan rolld detected: what builds the image, so nothing about the build is hidden. */
function PlanView({ state }: { state: { plan?: Plan; error?: string } | null }) {
  if (!state)
    return (
      <>
        <Label>Build plan</Label>
        <p className="text-[11.5px] text-subtle">Pick a repository to see how it builds.</p>
      </>
    );
  if (state.error)
    return (
      <>
        <Label>Build plan</Label>
        <p className="text-[11.5px] leading-relaxed text-danger">{state.error}</p>
      </>
    );
  if (!state.plan)
    return (
      <>
        <Label>Build plan</Label>
        <p className="text-[11.5px] text-subtle">
          <span className="shimmer">Reading the tree…</span>
        </p>
      </>
    );
  return <PlanDetails plan={state.plan} />;
}

export function PlanDetails({ plan }: { plan: Plan }) {
  const rows: [string, string][] = [
    ["provider", plan.provider + (plan.language_version ? ` ${plan.language_version}` : "")],
    ["framework", plan.framework ?? "—"],
    ...(plan.install ? ([["install", plan.install]] as [string, string][]) : []),
    ...(plan.build ? ([["build", plan.build]] as [string, string][]) : []),
    ["start", plan.start ?? "the image's command"],
    ["port", String(plan.port)],
  ];
  return (
    <>
      <Label>Build plan</Label>
      <Rows rows={rows} />
      {plan.notes.length > 0 && (
        <ul className="mt-2 space-y-0.5 text-[11px] text-subtle">
          {plan.notes.map((n) => (
            <li key={n}>· {n}</li>
          ))}
        </ul>
      )}
      <Dockerfile text={plan.dockerfile} generated={plan.provider !== "dockerfile"} />
    </>
  );
}

/** The Dockerfile behind a build, in a disclosure that animates its height. */
export function Dockerfile({ text, generated }: { text: string; generated: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="mt-2 rounded-lg border border-line-2 bg-raised inset-shadow-highlight">
      <button onClick={() => setOpen((o) => !o)} aria-expanded={open} className="flex w-full items-center gap-1.5 px-2.5 py-1.5 text-left text-[11.5px] text-muted hover:text-fg">
        <ChevronRight className={`size-3.5 transition-transform duration-150 motion-reduce:transition-none ${open ? "rotate-90" : ""}`} />
        Dockerfile
        <span className="ml-auto font-mono text-[10.5px] text-subtle">{generated ? "generated" : "the repo's own"}</span>
      </button>
      <div className={`grid transition-[grid-template-rows] duration-200 ease-out-soft motion-reduce:transition-none ${open ? "grid-rows-[1fr]" : "grid-rows-[0fr]"}`}>
        <div className="overflow-hidden">
          <pre className="max-h-72 overflow-auto border-t border-line px-2.5 py-2 font-mono text-[10.5px] leading-relaxed whitespace-pre text-soft">{text}</pre>
        </div>
      </div>
    </div>
  );
}

/**
 * A GitHub repository as the source in one step: one roll code already mirrors is picked as it
 * is; another is imported first (roll code mirrors it from then on). Only when this rolld has a
 * GitHub App that can see repositories.
 */
function FromGithub({ onPicked }: { onPicked: (fullName: string) => void }) {
  const [installs, setInstalls] = useState<GithubInstallation[] | null>(null);
  const [owners, setOwners] = useState<Owner[]>([]);
  const [owner, setOwner] = useState("");
  const [pick, setPick] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        const app = await codeApi.github();
        if (!app.configured || !alive) return;
        const [i, o] = await Promise.all([codeApi.installations(), codeApi.owners()]);
        if (!alive) return;
        setInstalls(i);
        setOwners(o);
        setOwner(o[0]?.name ?? "");
      } catch {
        // No GitHub here (or not for this caller): the picker just doesn't show.
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const repos = (installs ?? []).flatMap((i) => i.repos.map((r) => ({ install: i, repo: r })));
  if (!repos.length) return null;
  const chosen = repos.find((r) => r.repo.full_name === pick);

  const go = async () => {
    if (!chosen) return;
    if (chosen.repo.connected_repo) return onPicked(chosen.repo.connected_repo);
    setBusy(true);
    setError(null);
    try {
      const made = await codeApi.createRepo({
        owner,
        name: chosen.repo.name,
        default_branch: chosen.repo.default_branch,
        visibility: chosen.repo.private ? "private" : "public",
        source: { github: { installation_id: chosen.install.id, owner: chosen.repo.owner, repo: chosen.repo.name } },
      });
      onPicked(made.full_name);
      setPick("");
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Label>
        Or from GitHub <span className="font-normal">roll code mirrors it</span>
      </Label>
      <div className="flex gap-1.5">
        <select value={pick} onChange={(e) => setPick(e.target.value)} className={`${field} min-w-0 flex-1`} aria-label="GitHub repository">
          <option value="">Pick a repository…</option>
          {repos.map(({ repo }) => (
            <option key={repo.full_name} value={repo.full_name}>
              {repo.full_name}
              {repo.connected_repo ? ` · in roll as ${repo.connected_repo}` : ""}
            </option>
          ))}
        </select>
        {chosen && !chosen.repo.connected_repo && owners.length > 1 && (
          <select value={owner} onChange={(e) => setOwner(e.target.value)} className={`${field} w-28`} aria-label="Into the owner">
            {owners.map((o) => (
              <option key={o.name} value={o.name}>
                {o.name}
              </option>
            ))}
          </select>
        )}
        <button type="button" onClick={() => void go()} disabled={!chosen || busy || (!chosen.repo.connected_repo && !owner)} className="shrink-0 rounded-md border border-line px-2 text-[11.5px] text-fg hover:bg-fg/[0.05] disabled:opacity-50">
          {busy ? "Importing…" : chosen?.repo.connected_repo ? "Use" : "Import"}
        </button>
      </div>
      {error && <p className="mt-1 text-[11px] text-danger">{error}</p>}
    </>
  );
}
