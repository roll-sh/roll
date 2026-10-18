"use client";

import { DotField, HorizonMark } from "@roll/brand";
import Link from "next/link";
import { usePathname } from "next/navigation";
import type { ReactNode } from "react";
import { ROLL_API } from "@/lib/queues-api";
import { AppShell } from "../app-shell";
import { useMarkState } from "../mark-state";
import { useLoadingMark } from "./hooks";
import { ErrorNote } from "./ui";

const tabs = [
  { label: "Overview", href: "/dashboard/queues" },
  { label: "Vhosts", href: "/dashboard/queues/vhosts" },
  { label: "Queues", href: "/dashboard/queues/queues" },
  { label: "Exchanges", href: "/dashboard/queues/exchanges" },
  { label: "Connections", href: "/dashboard/queues/connections" },
  { label: "Policies", href: "/dashboard/queues/policies" },
  { label: "Users", href: "/dashboard/queues/users" },
  { label: "Usage", href: "/dashboard/queues/usage" },
  { label: "Migrate", href: "/dashboard/queues/migrate" },
];

function Tabs() {
  const pathname = usePathname();
  return (
    <nav className="-mx-1 mt-5 flex gap-1 overflow-x-auto border-b border-line" aria-label="Queues sections">
      {tabs.map((t) => {
        const active = t.href === "/dashboard/queues" ? pathname === t.href : pathname.startsWith(t.href);
        return (
          <Link
            key={t.href}
            href={t.href}
            aria-current={active ? "page" : undefined}
            className={`-mb-px shrink-0 border-b px-2.5 pt-1 pb-2.5 text-[13px] transition ${
              active ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
            }`}
          >
            {t.label}
          </Link>
        );
      })}
    </nav>
  );
}

/** Frame shared by every queues page: app shell, title, section tabs, offline and error states. */
export function QueuesShell({
  title,
  description,
  crumb,
  actions,
  meta,
  loading = false,
  offline = false,
  error,
  onDismissError,
  children,
}: {
  title: ReactNode;
  description?: ReactNode;
  crumb?: string;
  actions?: ReactNode;
  meta?: ReactNode;
  loading?: boolean;
  offline?: boolean;
  error?: string | null;
  onDismissError?: () => void;
  children: ReactNode;
}) {
  useLoadingMark(loading && !offline);
  return (
    <AppShell crumb={crumb ? `queues / ${crumb}` : "queues"} actions={offline ? undefined : actions}>
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0">
          <h1 className="truncate text-2xl font-semibold tracking-[-0.04em]">{title}</h1>
          {description && <p className="mt-1 text-[13px] text-muted">{description}</p>}
        </div>
        {meta && <div className="font-mono text-[11px] text-subtle">{meta}</div>}
      </div>
      <Tabs />
      {error && (
        <div className="mt-4">
          <ErrorNote error={error} onClose={onDismissError} />
        </div>
      )}
      {offline ? (
        <QueuesOffline />
      ) : loading ? (
        <p className="mt-10 text-[13px] text-subtle">
          <span className="shimmer">Connecting to roll queues…</span>
        </p>
      ) : (
        <div className="mt-6 space-y-4">{children}</div>
      )}
    </AppShell>
  );
}

/** Shown when rolld (and so the queues API) can't be reached. */
export function QueuesOffline() {
  const { state } = useMarkState();
  return (
    <section className="relative isolate mt-6 overflow-hidden rounded-2xl border border-line bg-panel">
      <DotField spacing={18} radius={90} mode={state === "thinking" ? "thinking" : "idle"} className="-z-10 text-fg" />
      <div className="flex flex-col items-start gap-3 p-8">
        <HorizonMark state="sleep" size={40} />
        <h2 className="text-[15px] font-medium">roll queues isn&apos;t reachable</h2>
        <p className="max-w-lg text-[13px] text-muted">
          The dashboard talks to the queues API on the local control plane at{" "}
          <code className="font-mono text-fg">{ROLL_API}</code>. Start rolld from the repo root. Queues start with it
          (AMQP on <span className="font-mono">127.0.0.1:55672</span>, user <span className="font-mono">guest</span>):
        </p>
        <pre className="rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12px]">cargo run -p rolld</pre>
        <p className="max-w-lg text-[12px] text-subtle">
          Running it elsewhere? Set <code className="font-mono">ROLL_API=host:port</code> for rolld and{" "}
          <code className="font-mono">NEXT_PUBLIC_ROLL_API=http://host:port</code> for the dashboard. Make sure
          queues aren&apos;t turned off with <code className="font-mono">--no-queues</code> or{" "}
          <code className="font-mono">ROLL_QUEUES_DISABLED</code>.
        </p>
      </div>
    </section>
  );
}
