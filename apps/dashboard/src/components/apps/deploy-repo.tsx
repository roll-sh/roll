import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { workspace } from "@/lib/workspace-store";
import { input, Panel, primary } from "../ui";
import { ImportPane } from "./import";

/**
 * A "deploy to roll" link (`/dashboard/deploy?repo=owner/name`, Heroku's button, Railway's
 * templates): the repo's roll.toml made into a new project. Its services, databases and links as
 * the file says; a `generate = "secret"` variable made once, an `ask` one asked here. The same
 * import as New service's "From a file", read from the repo.
 */
export function DeployRepo({ repo }: { repo?: string }) {
  const navigate = useNavigate();
  const [name, setName] = useState(() => (repo?.split("/")[1] ?? "").toLowerCase().replace(/[^a-z0-9-]+/g, "-").slice(0, 40));
  const [project, setProject] = useState<WorkspaceProject | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (!repo || !/^[\w.-]+\/[\w.-]+$/.test(repo)) {
    return (
      <div className="mx-auto max-w-2xl px-6 py-16">
        <h1 className="text-[22px] font-semibold tracking-[-0.04em]">Deploy to roll</h1>
        <p className="mt-2 text-[13px] text-muted">
          A deploy link names a repo in roll code: <code className="font-mono text-fg">/dashboard/deploy?repo=owner/name</code>. Its roll.toml says what to make.
        </p>
      </div>
    );
  }
  return (
    <div className="mx-auto max-w-2xl space-y-5 px-6 py-12">
      <div>
        <h1 className="text-[22px] font-semibold tracking-[-0.04em]">
          Deploy <span className="font-mono text-[19px]">{repo}</span>
        </h1>
        <p className="mt-1.5 text-[13px] text-muted">Its roll.toml becomes a project of yours: the services, databases and links it declares, deployed. What it asks is asked below; its secrets are made for you.</p>
      </div>
      {!project ? (
        <Panel title="A new project">
          <form
            className="flex flex-wrap items-center gap-2 px-5 py-4"
            onSubmit={(e) => {
              e.preventDefault();
              setBusy(true);
              setError(null);
              workspace
                .createProject(name.trim(), { description: `Deployed from ${repo}` })
                .then(setProject, (err: unknown) => setError(err instanceof Error ? err.message : String(err)))
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
      ) : (
        <Panel title={`Into ${project.name}`}>
          <div className="px-5 py-4">
            <ImportPane project={project} spot={{ x: 48, y: 48 }} repo={repo} onDone={() => void navigate({ to: "/dashboard/projects/$id", params: { id: project.id } })} />
          </div>
        </Panel>
      )}
    </div>
  );
}
