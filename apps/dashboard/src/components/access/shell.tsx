import { KeyRound, Plus } from "lucide-react";
import type { ReactNode } from "react";
import { AppShell } from "../app-shell";
import { Link } from "../link";
import { primary } from "../ui";
import { usePendingCount } from "./use-access";

export { usePoll } from "./use-access";

export type AccessTab = "overview" | "approvals" | "audit" | "new";

const TABS: { id: AccessTab; label: string; href: string }[] = [
  { id: "overview", label: "Connect & tokens", href: "/dashboard/access" },
  { id: "approvals", label: "Approvals", href: "/dashboard/access/approvals" },
  { id: "audit", label: "Audit log", href: "/dashboard/access/audit" },
];

/** The "MCP & access" frame: its tabs, and a "New token" button on every view but the builder. */
export function AccessShell({ tab, children }: { tab: AccessTab; children: ReactNode }) {
  const pending = usePendingCount();
  const crumb = (
    <>
      <KeyRound className="size-3.5 text-muted" />
      <Link href="/dashboard/access" className={tab === "overview" ? "" : "text-muted hover:text-fg"}>
        MCP &amp; access
      </Link>
      {tab === "new" && (
        <>
          <span className="text-subtle">/</span> New token
        </>
      )}
    </>
  );
  const actions =
    tab === "new" ? undefined : (
      <Link href="/dashboard/access/new" className={primary}>
        <Plus className="size-3.5" /> New token
      </Link>
    );
  return (
    <AppShell crumb={crumb} actions={actions}>
      {tab !== "new" && (
        <nav className="mb-6 flex gap-1 border-b border-line" aria-label="MCP and access">
          {TABS.map((t) => (
            <Link
              key={t.id}
              href={t.href}
              aria-current={t.id === tab ? "page" : undefined}
              className={`-mb-px flex items-center gap-1.5 border-b px-3 py-2 text-[13px] transition-colors duration-150 ${
                t.id === tab ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
              }`}
            >
              {t.label}
              {t.id === "approvals" && pending > 0 && (
                <span className="rounded-full bg-accent px-1.5 font-mono text-[10px] font-semibold text-white tabular-nums">{pending}</span>
              )}
            </Link>
          ))}
        </nav>
      )}
      {children}
    </AppShell>
  );
}

/** "in 3d", "2h ago", "never". */
export function relative(ms: number | null, now = Date.now()): string {
  if (ms === null) return "never";
  const d = ms - now;
  const a = Math.abs(d) / 1000;
  const n = a < 60 ? `${Math.round(a)}s` : a < 3600 ? `${Math.round(a / 60)}m` : a < 86400 ? `${Math.round(a / 3600)}h` : `${Math.round(a / 86400)}d`;
  return d >= 0 ? `in ${n}` : `${n} ago`;
}
