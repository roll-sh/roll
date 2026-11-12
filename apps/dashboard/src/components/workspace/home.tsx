import { ArrowUpRight } from "lucide-react";
import { Link } from "@/components/link";
import { formatUsd } from "@/lib/roll-api";
import { KIND_LABEL, monthOf, projectMonth, resourceKey } from "@/lib/workspace-resources";
import { spendOf, useWorkspace } from "@/lib/workspace-store";
import { AppShell } from "../app-shell";
import { KindTile, MiniMap } from "./kind";

/** `/dashboard`: every project at a glance, this month's spend, and what's in no project yet. */
export function Home() {
  const ws = useWorkspace();
  const month = monthOf(new Date());
  const total = spendOf(ws, ws.projects.flatMap((p) => p.resources));
  const placed = new Set(ws.projects.flatMap((p) => p.resources.map(resourceKey)));
  const loose = [...ws.inventory.values()].filter((v) => !placed.has(resourceKey(v)));

  return (
    <AppShell crumb="Home">
      <section className="rise">
        <h1 className="text-[20px] font-semibold tracking-[-0.045em]">Projects</h1>
        <p className="mt-0.5 text-[12.5px] text-muted">Each project groups its databases, caches, streams and buckets on one canvas.</p>
        <div className="mt-4 grid gap-2.5 sm:grid-cols-2 lg:grid-cols-3">
          {ws.projects.map((p) => {
            const s = spendOf(ws, p.resources);
            return (
              <Link
                key={p.id}
                href={`/dashboard/projects/${p.id}`}
                className="group rounded-xl border border-line-2 bg-sheet p-3 shadow-card inset-shadow-highlight transition-[border-color,box-shadow] duration-150 hover:border-line-3 hover:shadow-card-hover"
              >
                <div className="flex items-center gap-2.5">
                  <MiniMap project={p} staged={ws.staged[p.id]} size={32} />
                  <div className="min-w-0 flex-1 leading-tight">
                    <div className="truncate text-[13px] font-semibold tracking-[-0.015em]">{p.name}</div>
                    <div className="font-mono text-[10.5px] text-subtle">
                      {p.resources.length} {p.resources.length === 1 ? "resource" : "resources"}
                    </div>
                  </div>
                  <ArrowUpRight className="size-3.5 text-subtle transition-colors group-hover:text-fg" />
                </div>
                <div className="mt-3 flex flex-wrap gap-1">
                  {p.resources.slice(0, 6).map((r) => (
                    <span key={resourceKey(r)} className="flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 text-[10.5px] text-muted">
                      {KIND_LABEL[r.kind]}
                    </span>
                  ))}
                </div>
                <div className="mt-3 flex justify-between border-t border-line pt-2 font-mono text-[11px] text-subtle">
                  <span>{formatUsd(s.usd)} so far</span>
                  <span>≈ {formatUsd(projectMonth(s.usd, month))}</span>
                </div>
              </Link>
            );
          })}
          {ws.status === "ready" && !ws.projects.length && (
            <p className="rounded-xl border border-dashed border-line-2 p-4 text-[12.5px] text-muted">
              No projects yet. Make one with <b className="font-medium text-fg">+</b> beside Projects in the sidebar.
            </p>
          )}
        </div>
      </section>

      <section id="usage" className="rise mt-10 scroll-mt-16">
        <h2 className="text-[15px] font-semibold tracking-[-0.03em]">{month.label}</h2>
        <div className="mt-3 grid grid-cols-3 gap-2.5">
          <Stat label="So far" value={formatUsd(total.usd)} note={`day ${month.elapsed} of ${month.days}`} />
          <Stat label="Projected" value={`≈ ${formatUsd(projectMonth(total.usd, month))}`} note={`by ${month.end}, at this rate`} />
          <Stat label="Projects" value={String(ws.projects.length)} note={`${placed.size} resources`} />
        </div>
        {ws.projects.length > 0 && (
          <div className="mt-3 rounded-xl border border-line-2 bg-sheet inset-shadow-highlight">
            <div className="flex px-3 py-2 text-[11px] font-medium text-subtle">
              <span className="flex-1">Project</span>
              <span className="w-24 text-right">so far</span>
              <span className="w-24 text-right">projected</span>
            </div>
            {ws.projects.map((p) => {
              const s = spendOf(ws, p.resources);
              return (
                <div key={p.id} className="flex items-center border-t border-line px-3 py-2 text-[12.5px]">
                  <span className="flex-1 truncate">{p.name}</span>
                  <span className="w-24 text-right font-mono text-[12px]">{s.pending && !s.usd ? "…" : formatUsd(s.usd)}</span>
                  <span className="w-24 text-right font-mono text-[12px] text-subtle">{formatUsd(projectMonth(s.usd, month))}</span>
                </div>
              );
            })}
          </div>
        )}
      </section>

      {loose.length > 0 && (
        <section className="rise mt-10">
          <h2 className="text-[15px] font-semibold tracking-[-0.03em]">Not in a project</h2>
          <p className="mt-0.5 text-[12.5px] text-muted">Add these to a project from its canvas (Add, then pick one).</p>
          <div className="mt-3 rounded-xl border border-line-2 bg-sheet inset-shadow-highlight">
            {loose.map((v) => (
              <Link key={resourceKey(v)} href={v.href} className="flex items-center gap-2.5 border-t border-line px-3 py-2 first:border-t-0 hover:bg-white/[0.02]">
                <KindTile kind={v.kind} />
                <span className="min-w-0 flex-1 leading-tight">
                  <span className="block truncate text-[12.5px] font-medium">{v.name}</span>
                  <span className="block truncate font-mono text-[10.5px] text-subtle">{v.sub}</span>
                </span>
                <span className="text-[11px] text-muted">{KIND_LABEL[v.kind]}</span>
              </Link>
            ))}
          </div>
        </section>
      )}
    </AppShell>
  );
}

function Stat({ label, value, note }: { label: string; value: string; note: string }) {
  return (
    <div className="rounded-xl border border-line-2 bg-sheet px-3 py-2.5 inset-shadow-highlight">
      <div className="text-[11px] text-subtle">{label}</div>
      <div className="mt-0.5 font-mono text-[18px] font-semibold tracking-[-0.04em]">{value}</div>
      <div className="font-mono text-[10.5px] text-subtle">{note}</div>
    </div>
  );
}
