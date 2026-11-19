import { Check, Rocket } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, describePending } from "@/lib/apps-api";
import { KIND_LABEL, resourceKey } from "@/lib/workspace-resources";
import { refreshInventory, workspace, type WorkspaceState } from "@/lib/workspace-store";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { Dialog } from "../agents/ui/dialog";

/** One service environment with changes its live deployment doesn't run with. */
interface Change {
  service: string;
  name: string;
  env: string;
}

type Outcome = { state: "deploying" } | { state: "started" } | { state: "failed"; error: string };

const keyOf = (c: Change) => `${c.service}/${c.env}`;

/** What's staged in a project: new resources to make, and each service environment to redeploy. */
export function stagedChanges(ws: WorkspaceState, project: WorkspaceProject): Change[] {
  return project.resources.flatMap((r) => {
    const v = ws.inventory.get(resourceKey(r));
    return v?.kind === "service" ? (v.toDeploy ?? []).map((env) => ({ service: v.ref, name: v.name, env })) : [];
  });
}

/**
 * Railway's "Deploy N changes": everything staged in the project in one sheet, the new resources
 * to make and each environment whose settings, variables, links or the project's secrets changed
 * since its live deployment (what changed, named). Deploy makes the new ones and fires each picked
 * environment's own redeploy (the same image, no build; its permissions are that service's), and
 * says how each went.
 */
export function DeployChanges({ ws, project, open, onClose }: { ws: WorkspaceState; project: WorkspaceProject; open: boolean; onClose: () => void }) {
  const staged = (ws.staged[project.id] ?? []).filter((s) => s.phase !== "creating");
  const [changes, setChanges] = useState<Change[]>([]);
  const [what, setWhat] = useState<Record<string, string | null>>({});
  const [skip, setSkip] = useState<Set<string>>(new Set());
  const [outcome, setOutcome] = useState<Record<string, Outcome>>({});
  const busy = Object.values(outcome).some((o) => o.state === "deploying");

  // Frozen when it opens: a list that moved under the pointer would deploy what wasn't read.
  useEffect(() => {
    if (!open) return;
    const list = stagedChanges(ws, project);
    setChanges(list);
    setSkip(new Set());
    setOutcome({});
    setWhat({});
    let live = true;
    for (const c of list)
      appsApi.pending(c.service, c.env).then(
        (p) => live && setWhat((w) => ({ ...w, [keyOf(c)]: describePending(p) })),
        () => live && setWhat((w) => ({ ...w, [keyOf(c)]: null })),
      );
    return () => {
      live = false;
    };
  }, [open]);

  const picked = changes.filter((c) => !skip.has(keyOf(c)) && outcome[keyOf(c)]?.state !== "started");
  const done = Object.keys(outcome).length > 0 && !busy;
  const count = picked.length + staged.length;

  const deploy = async () => {
    setOutcome((o) => ({ ...o, ...Object.fromEntries(picked.map((c) => [keyOf(c), { state: "deploying" } as Outcome])) }));
    await Promise.all([
      staged.length ? workspace.deploy(project.id) : null,
      ...picked.map((c) =>
        appsApi.redeploy(c.service, c.env).then(
          () => setOutcome((o) => ({ ...o, [keyOf(c)]: { state: "started" } })),
          (e) => setOutcome((o) => ({ ...o, [keyOf(c)]: { state: "failed", error: e instanceof Error ? e.message : String(e) } })),
        ),
      ),
    ]);
    await refreshInventory().catch(() => {});
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      busy={busy}
      title={done ? "Deploying" : `Deploy ${count} ${count === 1 ? "change" : "changes"}`}
      lead={done ? "Each redeploy runs on its own; its service's Deployments tab follows it." : "New resources are made, and each picked environment is redeployed: the same image, no build."}
      footer={
        <>
          <button type="button" onClick={onClose} disabled={busy} className="h-8 rounded-md px-3 text-[12.5px] text-muted hover:bg-white/[0.05] hover:text-fg disabled:opacity-50">
            {done ? "Close" : "Cancel"}
          </button>
          {!(done && count === 0) && (
            <button
              type="button"
              onClick={() => void deploy()}
              disabled={busy || count === 0}
              className="flex h-8 items-center gap-1.5 rounded-md bg-fg px-3 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-50"
            >
              <Rocket className="size-3.5" /> {busy ? "Deploying…" : done ? `Retry ${count}` : `Deploy ${count}`}
            </button>
          )}
        </>
      }
    >
      <ul className="divide-y divide-line overflow-hidden rounded-xl border border-line-2 bg-canvas text-[12.5px]">
        {staged.map((s) => (
          <li key={s.id} className="flex items-center gap-2.5 px-3 py-2.5">
            <i className="size-1.5 shrink-0 rounded-full shadow-[inset_0_0_0_1.25px_var(--accent)]" />
            <span className="min-w-0 flex-1 truncate">
              <span className="font-medium">{s.name}</span> <span className="text-subtle">· new {KIND_LABEL[s.kind]}</span>
            </span>
            {s.error && <span className="truncate text-[11.5px] text-danger">{s.error}</span>}
          </li>
        ))}
        {changes.map((c) => {
          const k = keyOf(c);
          const o = outcome[k];
          return (
            <li key={k} className="flex items-start gap-2.5 px-3 py-2.5">
              {o ? (
                <span className="mt-0.5 grid size-3.5 shrink-0 place-items-center">
                  {o.state === "started" ? <Check className="size-3.5 text-accent" /> : <i className={`size-1.5 rounded-full ${o.state === "failed" ? "bg-danger" : "pulse-dot bg-accent"}`} />}
                </span>
              ) : (
                <input
                  type="checkbox"
                  className="mt-0.5 size-3.5 shrink-0 accent-[var(--accent)]"
                  checked={!skip.has(k)}
                  aria-label={`Redeploy ${c.name} in ${c.env}`}
                  onChange={(e) =>
                    setSkip((s) => {
                      const n = new Set(s);
                      if (e.target.checked) n.delete(k);
                      else n.add(k);
                      return n;
                    })
                  }
                />
              )}
              <span className="min-w-0 flex-1">
                <span className="block truncate">
                  <span className="font-medium">{c.name}</span> <span className="text-subtle">· {c.env}</span>
                </span>
                <span className="block truncate font-mono text-[11px] text-muted">
                  {o?.state === "failed" ? <span className="text-danger">{o.error}</span> : o?.state === "started" ? "redeploying" : (what[k] ?? (k in what ? "a link or the project's secrets changed" : "…"))}
                </span>
              </span>
            </li>
          );
        })}
      </ul>
    </Dialog>
  );
}
