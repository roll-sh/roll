import { useNavigate } from "@tanstack/react-router";
import { Copy } from "lucide-react";
import { useState } from "react";
import { appsApi, SERVICE_NAME, type Link, type NewService, type Service, type Variables } from "@/lib/apps-api";
import { freeSpot, service as serviceView } from "@/lib/workspace-resources";
import { useWorkspace, workspace } from "@/lib/workspace-store";
import { btn, input, Panel } from "../ui";
import type { Runner } from "./shared";

/** A free name for the copy: `<name>-copy`, then `-copy-2`… (within 32). */
export function copyName(name: string, taken: Set<string>): string {
  for (let i = 1; i < 100; i++) {
    const tail = i === 1 ? "-copy" : `-copy-${i}`;
    const n = `${name.slice(0, 32 - tail.length).replace(/-+$/, "")}${tail}`;
    if (!taken.has(n)) return n;
  }
  return name;
}

/**
 * What a copy of `s` is made with (Railway's Duplicate): its source, build and run settings, its
 * links, and production's own variables (a reference's template as it is). Sealed values are
 * never shown, so they aren't copied: they're named to set again. Nothing else goes: other
 * environments, domains, a registry login.
 */
export function duplicateOf(s: Service, name: string, vars: Variables, values: Record<string, string>, links: Link[]): { body: NewService; sealed: string[] } {
  const variables: Record<string, string> = {};
  const sealed: string[] = [];
  for (const v of vars.vars) {
    if (v.sealed) {
      if (v.source === "service" || v.source === "reference") sealed.push(v.name);
      continue;
    }
    if (v.source === "reference" && v.reference) variables[v.name] = v.reference;
    else if (v.source === "service" && v.name in values) variables[v.name] = values[v.name]!;
  }
  const body: NewService = {
    project: s.project,
    name,
    source: s.source,
    build: s.build,
    run: s.run,
    ...(Object.keys(variables).length ? { variables } : {}),
    ...(links.length ? { links: links.map((l) => ({ kind: l.kind, ref: l.ref, prefix: l.prefix })) } : {}),
  };
  return { body, sealed };
}

/** Settings' Duplicate: a copy of the service in its project, beside it on the canvas. */
export function DuplicatePanel({ service, runner }: { service: Service; runner: Runner }) {
  const ws = useWorkspace();
  const navigate = useNavigate();
  const project = ws.projects.find((p) => p.id === service.project);
  const taken = new Set([...ws.inventory.values()].filter((v) => v.kind === "service").map((v) => v.name));
  const [name, setName] = useState(() => copyName(service.name, taken));
  const [made, setMade] = useState<{ id: string; name: string; sealed: string[] } | null>(null);
  const ok = SERVICE_NAME.test(name) && name !== service.name;

  const duplicate = () =>
    runner.run("duplicate", async () => {
      const [vars, values, links] = await Promise.all([appsApi.variables(service.id, "production"), appsApi.reveal(service.id, "production"), appsApi.links(service.id)]);
      const at = freeSpot(project?.resources ?? []);
      const { body, sealed } = duplicateOf(service, name, vars, values, links);
      const s = await appsApi.create({ ...body, ...at });
      if (project) workspace.added(project.id, serviceView(s), at.x, at.y);
      setMade({ id: s.id, name: s.name, sealed });
    });

  return (
    <Panel title="Duplicate">
      <div className="space-y-3 p-5">
        <p className="text-[13px] text-muted">
          A new service in this project with the same source, build and run settings, links and production variables. {service.source.kind === "upload" ? "It deploys with `roll up`." : "It deploys at once."} Sealed variables, other environments, domains and a registry login stay here.
        </p>
        {made ? (
          <div className="rise flex flex-wrap items-center gap-3 text-[13px]">
            <span>
              Made <span className="font-mono">{made.name}</span>
              {made.sealed.length > 0 && (
                <>
                  : set its sealed <span className="font-mono">{made.sealed.join(", ")}</span> again
                </>
              )}
              .
            </span>
            <button type="button" className={btn} onClick={() => void navigate({ to: "/dashboard/apps/$id", params: { id: made.id }, search: made.sealed.length ? { tab: "variables" } : {} })}>
              Open it
            </button>
          </div>
        ) : (
          <div className="flex flex-wrap items-center gap-2">
            <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} aria-label="The copy's name" spellCheck={false} className={`${input} w-64 font-mono text-[12px]`} />
            <button type="button" className={btn} disabled={!ok || runner.busy !== null} onClick={() => void duplicate()}>
              <Copy className="size-3.5" /> {runner.busy === "duplicate" ? <span className="shimmer">Duplicating…</span> : "Duplicate"}
            </button>
          </div>
        )}
      </div>
    </Panel>
  );
}
