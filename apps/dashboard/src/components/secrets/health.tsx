import { CircleCheck } from "lucide-react";
import { Link } from "@/components/link";
import type { Finding } from "@/lib/secrets-api";
import { secretsHref, useSecrets } from "./secrets-page";
import { card, ErrorNote, Section, useLoad } from "./ui";

const ISSUES: { issue: Finding["issue"]; title: string; what: string; serious: boolean }[] = [
  { issue: "expired", title: "Expired", what: "Their values stopped working outside roll.", serious: true },
  { issue: "missing", title: "Missing", what: "Required keys an environment doesn't define.", serious: true },
  { issue: "exposed", title: "Exposed", what: "A protected environment's value, also set where more people and less vetted code read it.", serious: true },
  { issue: "weak", title: "Weak", what: "Credentials easy to guess. Edit one and Generate a new value.", serious: false },
  { issue: "expiring", title: "Expiring", what: "Values that stop working outside roll within two weeks.", serious: false },
  { issue: "stale", title: "Stale", what: "Unchanged for longer than their environment allows.", serious: false },
  { issue: "unread", title: "Unread", what: "Nobody revealed, exported, resolved or synced them in 90 days: still needed?", serious: false },
];

/** Everything insights flag, in every environment at once, worst first (docs/secrets.md, "Health"). */
export function HealthView() {
  const { api, project, standard } = useSecrets();
  const found = useLoad(() => api.health(), [api]);
  const name = (slug: string) => standard.find((e) => e.slug === slug)?.name ?? slug;
  if (found.error) return <ErrorNote error={found.error} className="m-4" />;
  if (!found.data) return null;
  return (
    <div className="mx-auto max-w-4xl space-y-6 px-4 py-5">
      {found.data.length === 0 && (
        <p className="flex items-center gap-2 rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12.5px] text-muted">
          <CircleCheck className="size-4 text-ok" /> Nothing to fix in {project.name}: no expired, missing, exposed, weak, expiring, stale or unread keys.
        </p>
      )}
      {ISSUES.map(({ issue, title, what, serious }) => {
        const rows = found.data!.filter((f) => f.issue === issue);
        if (!rows.length) return null;
        return (
          <Section
            key={issue}
            title={
              <span className="flex items-center gap-2">
                <span className={serious ? "text-danger" : undefined}>{title}</span>
                <span className="grid h-4 min-w-4 place-items-center rounded-full bg-fg/[0.06] px-1 font-mono text-[10px] text-muted tabular-nums">{rows.length}</span>
              </span>
            }
          >
            <p className="mb-2 text-[11.5px] text-muted">{what}</p>
            <div className={`${card} divide-y divide-line`}>
              {rows.map((f) => (
                <Link
                  key={`${f.environment}/${f.path}/${f.key}`}
                  href={secretsHref(project.id, { env: f.environment, path: f.path === "/" ? undefined : f.path, key: f.key })}
                  className="flex flex-wrap items-center gap-x-3 gap-y-0.5 px-3 py-2 text-[12.5px] transition-colors hover:bg-fg/[0.03]"
                >
                  <span className="font-mono text-fg">{f.key}</span>
                  <span className="text-subtle">
                    {name(f.environment)}
                    {f.path !== "/" && <span className="font-mono"> {f.path}</span>}
                  </span>
                  <span className={`ml-auto text-[11.5px] ${serious ? "text-danger" : "text-muted"}`}>{f.detail}</span>
                </Link>
              ))}
            </div>
          </Section>
        );
      })}
    </div>
  );
}
