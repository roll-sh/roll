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
