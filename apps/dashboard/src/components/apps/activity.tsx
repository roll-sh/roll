import { useEffect, useState } from "react";
import { appsApi, type AppEvent } from "@/lib/apps-api";
import { Panel } from "../ui";
import { clock } from "./shared";

/** One entry of the audit trail as a sentence: what changed (names, never values). */
export function describe(e: AppEvent): string {
  const d = e.data as Record<string, unknown>;
  const list = (v: unknown) => (Array.isArray(v) && v.length ? (v as string[]).join(", ") : null);
  const where = e.environment ? ` in ${e.environment}` : "";
  switch (e.kind) {
    case "audit.settings":
      return `changed ${list(d.changed) ?? "settings"}`;
    case "audit.variables": {
      const parts = [list(d.set) && `set ${list(d.set)}`, list(d.unset) && `removed ${list(d.unset)}`].filter(Boolean);
      return `${parts.join("; ") || "changed variables"}${where}`;
    }
    case "audit.variables.revealed":
      return `revealed the variables${where}`;
    case "audit.environment":
      return `changed ${list(d.changed) ?? "the environment"}${where}`;
    case "audit.environment.removed":
      return `deleted the environment ${e.environment}`;
    case "audit.domain.added":
      return `added ${String(d.hostname)}${where}`;
    case "audit.domain.removed":
      return `removed ${String(d.hostname)}${where}`;
    case "audit.domain.redirect":
      return d.redirect_to ? `redirected ${String(d.hostname)} to ${String(d.redirect_to)}` : `stopped redirecting ${String(d.hostname)}`;
    case "audit.link.added":
      return `linked ${String(d.kind)} ${String(d.ref)}`;
    case "audit.link.removed":
      return `unlinked ${String(d.kind)} ${String(d.ref)}`;
    case "audit.registry.set":
      return `set the registry login${d.server ? ` for ${String(d.server)}` : ""}`;
    case "audit.registry.removed":
      return "removed the registry login";
    // What roll did or saw (its timeline).
    case "deploy.active":
      return `put a deployment live${where}`;
    case "deploy.failed":
      return `saw a deploy fail${where}: ${String(d.error ?? "no reason given")}`;
    case "crash_loop":
      return `saw it crash ${String(d.crashes ?? "")} times in 10 minutes${where}: ${String(d.last_error ?? "")}`;
    case "cron.failed":
      return `saw a scheduled run fail${where}${d.exit_code != null ? ` (exit ${String(d.exit_code)})` : ""}`;
    case "job.failed":
      return `saw a job fail${where}${d.exit_code != null ? ` (exit ${String(d.exit_code)})` : ""}`;
    case "alert.memory":
    case "alert.errors":
    case "alert.latency":
    case "alert.log":
      return `alerted: ${e.kind.slice(6)} at ${String(d.value ?? d.count ?? "")} (the alert is at ${String(d.threshold ?? "")})${where}`;
    case "domain.active":
      return `put ${String(d.hostname)} live${where}`;
    case "domain.expiring":
      return `warned ${String(d.hostname)} is removed tomorrow unless it verifies`;
    case "domain.expired":
      return `removed ${String(d.hostname)}: unverified for 7 days`;
    case "domain.tls_failed":
      return `saw ${String(d.hostname)}'s certificate fail: ${String(d.error ?? "")}`;
    case "spend.alert":
    case "spend.limit":
      return e.kind === "spend.limit" ? "paused the project at its spending limit" : "alerted on the project's spend";
    default:
      return e.kind.replace(/^audit\./, "");
  }
}

/** A service's audit trail: who changed what (variables by name, never their values). */
export function ActivityPanel({ service }: { service: string }) {
  const [events, setEvents] = useState<AppEvent[] | null>(null);
  useEffect(() => {
    appsApi.activity(service).then(setEvents, () => setEvents([]));
  }, [service]);
  return (
    <Panel title="Activity">
      {events === null ? (
        <p className="px-5 py-4 text-[13px] text-subtle">
          <span className="shimmer">Loading…</span>
        </p>
      ) : events.length === 0 ? (
        <p className="px-5 py-4 text-[13px] text-muted">Nothing changed yet. Settings, variables, environments, domains and links show here (90 days).</p>
      ) : (
        <ul className="max-h-80 divide-y divide-line overflow-auto">
          {events.map((e) => {
            const who = typeof e.data?.actor === "string" ? (e.data.actor as string) : null;
            return (
              <li key={e.seq} className="flex gap-3 px-5 py-2 text-[12.5px]">
                <span className="shrink-0 font-mono text-[11.5px] text-subtle tabular-nums">{clock(e.at, true)}</span>
                <span className="min-w-0 flex-1">
                  <span className="text-fg">{who ?? "someone"}</span> <span className="text-muted">{describe(e)}</span>
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}

/**
 * A service's timeline (Render's Events): what roll did and saw (deploys, failures, crash loops,
 * alerts, domains) and what people changed, newest first, refreshed as it changes.
 */
export function EventsTab({ service, changed }: { service: string; changed: string }) {
  const [events, setEvents] = useState<AppEvent[] | null>(null);
  useEffect(() => {
    appsApi.activity(service, true).then(setEvents, () => setEvents([]));
  }, [service, changed]);
  return (
    <Panel title="Events">
      {events === null ? (
        <p className="px-5 py-4 text-[13px] text-subtle">
          <span className="shimmer">Loading…</span>
        </p>
      ) : events.length === 0 ? (
        <p className="px-5 py-4 text-[13px] text-muted">Nothing yet. Deploys, failures, crash loops, alerts, domains and changes people make show here.</p>
      ) : (
        <ul className="divide-y divide-line">
          {events.map((e) => {
            const bad = /failed|crash_loop|alert\.|expired|spend\.limit/.test(e.kind);
            const who = e.kind.startsWith("audit.") ? (typeof e.data?.actor === "string" ? (e.data.actor as string) : "someone") : "roll";
            return (
              <li key={e.seq} className="flex gap-3 px-5 py-2.5 text-[12.5px]">
                <span className="shrink-0 font-mono text-[11.5px] text-subtle tabular-nums">{clock(e.at, true)}</span>
                <i className={`mt-1.5 size-1.5 shrink-0 rounded-full ${bad ? "bg-danger" : who === "roll" ? "bg-subtle" : "bg-accent"}`} />
                <span className="min-w-0 flex-1 break-words">
                  <span className="text-fg">{who}</span> <span className="text-muted">{describe(e)}</span>
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}

