import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { template as templateOf, TEMPLATES, type Template } from "@/lib/templates";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { useWorkspace, workspace } from "@/lib/workspace-store";
import { input, Panel, primary } from "../ui";
import { ImportPane } from "./import";

/**
 * A "deploy to roll" link (`/dashboard/deploy?repo=owner/name`, Heroku's button, Railway's
 * templates): the repo's roll.toml made into a new project. Its services, databases and links as
 * the file says; a `generate = "secret"` variable made once, an `ask` one asked here. The same
 * import as New service's "From a file", read from the repo. `?template=umami` is a starter's
 * file instead, and `&project=<id>` applies it to a project you have rather than a new one.
 */
export function DeployRepo({ repo, template: templateId, project: into }: { repo?: string; template?: string; project?: string }) {
  const navigate = useNavigate();
  const ws = useWorkspace();
  const tpl = templateOf(templateId);
  const [name, setName] = useState(() => ((tpl?.id ?? repo?.split("/")[1]) ?? "").toLowerCase().replace(/[^a-z0-9-]+/g, "-").slice(0, 40));
  const [made, setMade] = useState<WorkspaceProject | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const project = made ?? (into ? ws.projects.find((p) => p.id === into) ?? null : null);
  const repoOk = !!repo && /^[\w.-]+\/[\w.-]+$/.test(repo);
  if (!repoOk && !tpl) return <Gallery />;
  const what = tpl ? tpl.name : repo!;
  return (
    <div className="mx-auto max-w-2xl space-y-5 px-6 py-12">
      <div>
        <h1 className="text-[22px] font-semibold tracking-[-0.04em]">
          Deploy <span className={tpl ? "" : "font-mono text-[19px]"}>{what}</span>
        </h1>
        <p className="mt-1.5 text-[13px] text-muted">
          {tpl ? `${tpl.blurb}, from its public image, with ${tpl.with}.` : "Its roll.toml becomes a project of yours: the services, databases and links it declares, deployed."} What it asks is asked below; its
          secrets are made for you.
        </p>
      </div>
      {!project ? (
        into && ws.status !== "ready" ? (
          <p className="text-[13px] text-subtle">
            <span className="shimmer">Loading the project…</span>
          </p>
        ) : (
          <Panel title="A new project">
            <form
              className="flex flex-wrap items-center gap-2 px-5 py-4"
              onSubmit={(e) => {
                e.preventDefault();
                setBusy(true);
                setError(null);
                workspace
                  .createProject(name.trim(), { description: `Deployed from ${tpl ? `the ${tpl.name} template` : repo}` })
                  .then(setMade, (err: unknown) => setError(err instanceof Error ? err.message : String(err)))
                  .finally(() => setBusy(false));
              }}
            >
              <input value={name} onChange={(e) => setName(e.target.value)} aria-label="Project name" placeholder="my-app" spellCheck={false} className={`${input} min-w-0 flex-1`} />
              <button type="submit" className={primary} disabled={!name.trim() || busy}>
                {busy ? "Making it…" : "Make the project"}
              </button>
              {error && <p className="w-full text-[12px] text-danger">{error}</p>}
            </form>
          </Panel>
        )
      ) : (
        <Panel title={`Into ${project.name}`}>
          <div className="px-5 py-4">
            <ImportPane
              project={project}
              spot={{ x: 48, y: 48 }}
              {...(tpl ? { template: tpl } : { repo })}
              onDone={() => void navigate({ to: "/dashboard/projects/$id", params: { id: project.id } })}
            />
          </div>
        </Panel>
      )}
    </div>
  );
}

/** The starter templates, and how a deploy link names a repo. */
function Gallery() {
  return (
    <div className="mx-auto max-w-2xl px-6 py-16">
      <h1 className="text-[22px] font-semibold tracking-[-0.04em]">Deploy to roll</h1>
      <p className="mt-2 text-[13px] text-muted">Start from a template, each a project with what it needs, made for you.</p>
      <TemplateList className="mt-5 grid grid-cols-1 gap-2 sm:grid-cols-2" />
      <p className="mt-6 text-[12.5px] text-subtle">
        A deploy link can also name a repo in roll code: <code className="font-mono text-muted">/dashboard/deploy?repo=owner/name</code>. Its roll.toml says what to make.
      </p>
    </div>
  );
}

/** The templates as cards linking to their deploy page (`project`: into that project). */
export function TemplateList({ project, className }: { project?: string; className?: string }) {
  return (
    <div className={className}>
      {TEMPLATES.map((t: Template) => (
        <Link
          key={t.id}
          to="/dashboard/deploy"
          search={{ template: t.id, ...(project ? { project } : {}) }}
          className="rounded-[10px] border border-line-2 bg-sheet px-3 py-2.5 text-left shadow-card inset-shadow-highlight transition-[border-color,box-shadow] duration-150 hover:border-line-3 hover:shadow-card-hover"
        >
          <div className="text-[12.5px] font-medium">{t.name}</div>
          <div className="text-[11px] text-subtle">
            {t.blurb} · {t.with}
          </div>
        </Link>
      ))}
    </div>
  );
}
