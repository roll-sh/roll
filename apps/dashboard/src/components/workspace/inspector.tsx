import { Check, Copy, ExternalLink, X } from "lucide-react";
import { useState, type ReactNode } from "react";
import { Link } from "@/components/link";
import { KIND_BLURB, KIND_LABEL, monthOf, projectMonth, usd, type ResourceView } from "@/lib/workspace-resources";
import type { Staged } from "@/lib/workspace-store";
import { KindTile } from "./kind";

/**
 * The sheet that slides in over the canvas's right side for the selected card: a resource's
 * address and facts, its spend this month, the way to its product's page; or a staged resource's
 * settings before Deploy makes it.
 */
export function Inspector({ open, onClose, children }: { open: boolean; onClose: () => void; children: ReactNode }) {
  return (
    <aside
      data-panel
      aria-hidden={!open}
      className={`absolute top-2 right-2 bottom-2 z-20 flex w-[340px] max-w-[calc(100%-1rem)] flex-col overflow-hidden rounded-xl border border-line-2 bg-sheet shadow-sheet inset-shadow-highlight transition-transform duration-300 ease-drawer motion-reduce:transition-none ${
        open ? "translate-x-0" : "pointer-events-none translate-x-[calc(100%+1rem)]"
      }`}
      onKeyDown={(e) => e.key === "Escape" && onClose()}
    >
      {children}
    </aside>
  );
}

export function Head({ tile, title, sub, actions, onClose }: { tile: ReactNode; title: string; sub: ReactNode; actions?: ReactNode; onClose: () => void }) {
  return (
    <div className="flex items-center gap-2.5 px-3 pt-3 pb-2.5">
      {tile}
      <div className="min-w-0 flex-1 leading-tight">
        <div className="truncate text-[14px] font-semibold tracking-[-0.025em]">{title}</div>
        <div className="mt-0.5 flex items-center gap-1.5 truncate text-[11px] text-muted">{sub}</div>
      </div>
      {actions}
      <button onClick={onClose} aria-label="Close" className="grid size-6 place-items-center rounded-md text-muted hover:bg-white/[0.06] hover:text-fg">
        <X className="size-3.5" />
      </button>
    </div>
  );
}

export function Tabs<T extends string>({ tabs, value, onChange }: { tabs: readonly T[]; value: T; onChange: (t: T) => void }) {
  return (
    <div className="flex gap-1 border-b border-line px-2">
      {tabs.map((t) => (
        <button
          key={t}
          onClick={() => onChange(t)}
          className={`relative px-1.5 pt-1 pb-2 text-[12px] transition-colors ${value === t ? "text-fg" : "text-muted hover:text-fg"}`}
        >
          {t}
          <span
            className={`absolute inset-x-1 -bottom-px h-[1.5px] rounded-full bg-fg transition-opacity duration-150 ${value === t ? "opacity-100" : "opacity-0"}`}
          />
        </button>
      ))}
    </div>
  );
}

export const Label = ({ children }: { children: ReactNode }) => (
  <div className="mt-4 mb-1.5 flex justify-between text-[11px] font-medium text-subtle first:mt-0">{children}</div>
);

export function Rows({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <div className="rounded-lg border border-line-2 bg-raised inset-shadow-highlight">
      {rows.map(([k, v]) => (
        <div key={k} className="flex items-center gap-3 border-t border-line px-2.5 py-1.5 first:border-t-0">
          <span className="w-24 shrink-0 font-mono text-[11px] text-muted">{k}</span>
          <span className="min-w-0 flex-1 truncate font-mono text-[11px]">{v}</span>
        </div>
      ))}
    </div>
  );
}

function CopyField({ shown, value }: { shown: string; value: string }) {
  const [done, setDone] = useState(false);
  return (
    <div className="flex items-center gap-2 rounded-lg border border-line-2 bg-raised py-1.5 pr-1.5 pl-2.5 inset-shadow-highlight">
      <span className="min-w-0 flex-1 truncate font-mono text-[11px]">{shown}</span>
      <button
        onClick={() =>
          navigator.clipboard.writeText(value).then(() => {
            setDone(true);
            setTimeout(() => setDone(false), 1200);
          })
        }
        aria-label="Copy"
        className="grid size-6 shrink-0 place-items-center rounded-md text-muted hover:bg-white/[0.06] hover:text-fg"
      >
        {done ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
      </button>
    </div>
  );
}

export function ResourceSheet({
  view,
  spent,
  onClose,
  onRemove,
}: {
  view: ResourceView;
  spent: number | null | undefined;
  onClose: () => void;
  onRemove: () => void;
}) {
  const [tab, setTab] = useState<"Overview" | "Usage">("Overview");
  const month = monthOf(new Date());
  return (
    <>
      <Head
        tile={<KindTile kind={view.kind} size={30} />}
        title={view.name}
        sub={
          <>
            <i className={`size-1.5 shrink-0 rounded-full ${view.state === "online" ? "bg-ok" : view.state === "missing" ? "bg-danger" : "bg-subtle"}`} />
            {view.stateLabel} · {KIND_LABEL[view.kind]}
          </>
        }
        actions={
          view.state !== "missing" && (
            <Link
              href={view.href}
              className="flex h-6 items-center gap-1 rounded-md border border-line-2 bg-raised px-2 text-[11px] text-muted inset-shadow-highlight hover:border-line-3 hover:text-fg"
            >
              Open <ExternalLink className="size-3" />
            </Link>
          )
        }
        onClose={onClose}
      />
      <Tabs tabs={["Overview", "Usage"] as const} value={tab} onChange={setTab} />
      <div key={tab} className="rise flex-1 overflow-y-auto p-3 [scrollbar-width:none]">
        {view.state === "missing" ? (
          <p className="text-[12px] leading-relaxed text-muted">
            {KIND_LABEL[view.kind]} <span className="font-mono text-fg">{view.ref}</span> isn't there any more. It was deleted outside this project.
          </p>
        ) : tab === "Overview" ? (
          <>
            {view.connect && (
              <>
                <Label>Connect</Label>
                <CopyField shown={view.connect} value={view.copy ?? view.connect} />
              </>
            )}
            <Label>Details</Label>
            <Rows rows={view.facts} />
          </>
        ) : (
          <>
            <Label>{month.label}</Label>
            <div className="grid grid-cols-2 gap-2">
              <Stat label="So far" value={spent == null ? "—" : usd(spent)} note={`day ${month.elapsed} of ${month.days}`} />
              <Stat label="Projected" value={spent == null ? "—" : `≈ ${usd(projectMonth(spent, month))}`} note={`by ${month.end}`} />
            </div>
            <p className="mt-3 text-[11.5px] leading-relaxed text-subtle">
              {spent === null
                ? `${KIND_LABEL[view.kind]} didn't report usage.`
                : "The projection carries this month's rate to its end. Idle time costs nothing: it scales to zero."}
            </p>
          </>
        )}
      </div>
      <div className="flex items-center justify-between border-t border-line px-3 py-2">
        <span className="text-[11px] text-subtle">Removing keeps the resource.</span>
        <button onClick={onRemove} className="rounded-md px-2 py-1 whitespace-nowrap text-[11.5px] text-muted hover:bg-white/[0.05] hover:text-fg">
          Remove from project
        </button>
      </div>
    </>
  );
}

export function Stat({ label, value, note }: { label: string; value: string; note: string }) {
  return (
    <div className="rounded-lg border border-line-2 bg-raised px-2.5 py-2 inset-shadow-highlight">
      <div className="text-[11px] text-subtle">{label}</div>
      <div className="mt-0.5 font-mono text-[15px] font-semibold tracking-[-0.03em]">{value}</div>
      <div className="font-mono text-[10px] text-subtle">{note}</div>
    </div>
  );
}

export function StagedSheet({
  staged,
  onClose,
  onRename,
  onRemove,
  onDeploy,
}: {
  staged: Staged;
  onClose: () => void;
  onRename: (name: string) => void;
  onRemove: () => void;
  onDeploy: () => void;
}) {
  return (
    <>
      <Head
        tile={<KindTile kind={staged.kind} size={30} />}
        title={staged.name}
        sub={
          <>
            <i className="size-1.5 rounded-full shadow-[inset_0_0_0_1.25px_var(--accent)]" />
            {staged.phase === "creating" ? "Creating" : staged.phase === "failed" ? "Failed" : "Staged"} · {KIND_LABEL[staged.kind]}
          </>
        }
        onClose={onClose}
      />
      <Tabs tabs={["Configure"] as const} value="Configure" onChange={() => {}} />
      <div className="flex-1 overflow-y-auto p-3">
        {staged.error && <p className="mb-3 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-2 text-[11.5px] text-danger">{staged.error}</p>}
        <Label>Name</Label>
        <input
          value={staged.name}
          disabled={staged.phase === "creating"}
          onChange={(e) => onRename(e.target.value.toLowerCase())}
          className="h-8 w-full rounded-lg border border-line-2 bg-canvas px-2.5 font-mono text-[12px] outline-none focus:border-accent/60"
        />
        <Label>Runs as</Label>
        <div className="rounded-lg border border-line-2 bg-raised px-2.5 py-2 inset-shadow-highlight">
          <div className="text-[12px]">Serverless</div>
          <div className="text-[11px] text-subtle">Scales to zero when idle; you pay for what it uses.</div>
        </div>
        <Label>What it is</Label>
        <p className="text-[11.5px] leading-relaxed text-muted">{KIND_BLURB[staged.kind]}. Its address and settings show here once it's made.</p>
      </div>
      <div className="flex items-center gap-2 border-t border-line px-3 py-2">
        <span className="mr-auto text-[11px] text-subtle">Nothing is made until you deploy.</span>
        <button onClick={onRemove} className="rounded-md px-2 py-1 text-[11.5px] text-muted hover:bg-white/[0.05] hover:text-fg">
          Remove
        </button>
        <button
          onClick={onDeploy}
          disabled={staged.phase === "creating"}
          className="h-7 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-50"
        >
          Deploy
        </button>
      </div>
    </>
  );
}
