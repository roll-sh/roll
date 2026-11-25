import { FileText } from "lucide-react";
import { useEffect, useState } from "react";
import { Link } from "@/components/link";
import { appsApi } from "@/lib/apps-api";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { resourceKey } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";

/** At most this many of the project's repos are read for a project file. */
const MAX_REPOS = 3;

export interface Drift {
  repo: string;
  /** What the repo's root roll.toml declares that the project doesn't have, by name. */
  missing: string[];
}

/**
 * What the project's repos' root roll.toml files declare (`[apps.*]`, `[databases.*]`) that the
 * project doesn't have yet (Render's Blueprint sync): read with the import a "deploy to roll"
 * link uses, by name, at each repo's default branch. Nothing is made here: the pill opens that
 * import's plan and Apply. A repo with no project file says nothing.
 */
export function useProjectDrift(project: WorkspaceProject): Drift[] {
  const ws = useWorkspace();
  const [drift, setDrift] = useState<Drift[]>([]);
  const resources = project.resources.map(resourceKey).sort().join(",");
  useEffect(() => {
    let alive = true;
    (async () => {
      // A product that's off or a read that fails says nothing, never an error on the page.
      try {
        const services = await appsApi.services(project.id).catch(() => []);
        const repos = [...new Set(services.flatMap((s) => (s.source.kind === "repo" ? [s.source.repo] : [])))].slice(0, MAX_REPOS);
        const names = new Set(services.map((s) => s.name));
        // The project's databases by kind and name, as the import matches them.
        const dbs = new Set(project.resources.map((r) => ws.inventory.get(resourceKey(r))).flatMap((v) => (v ? [`${v.kind}:${v.name}`] : [])));
        const out: Drift[] = [];
        for (const repo of repos) {
          const imp = await appsApi.importProject({ repo }).catch(() => null);
          if (!imp) continue;
          const missing = [...Object.keys(imp.apps).filter((n) => !names.has(n)), ...Object.entries(imp.databases).filter(([n, d]) => !dbs.has(`${d.kind}:${n}`)).map(([n]) => n)];
          if (missing.length) out.push({ repo, missing });
        }
        if (alive) setDrift(out);
      } catch {
        if (alive) setDrift([]);
      }
    })();
    return () => {
      alive = false;
    };
    // Read again when what's on the canvas changes (something was applied or removed).
  }, [project.id, resources, ws.inventory.size]);
  return drift;
}

/** The project's drift as header pills: each opens its repo's import, applied to this project. */
export function ProjectDrift({ project }: { project: WorkspaceProject }) {
  const drift = useProjectDrift(project);
  return (
    <>
      {drift.map((d) => (
        <Link
          key={d.repo}
          href={`/dashboard/deploy?repo=${encodeURIComponent(d.repo)}&project=${encodeURIComponent(project.id)}`}
          className="flex h-7 items-center gap-1.5 rounded-md border border-accent/40 px-2 text-[12px] text-muted transition-colors hover:text-fg"
          title={`${d.repo}'s roll.toml declares ${d.missing.join(", ")}, which this project doesn't have yet: review and apply`}
        >
          <FileText className="size-3.5 text-accent" strokeWidth={1.5} />
          <span className="max-md:hidden">roll.toml:</span>
          <span className="font-mono text-[11px] text-accent">{d.missing.length} new</span>
        </Link>
      ))}
    </>
  );
}
