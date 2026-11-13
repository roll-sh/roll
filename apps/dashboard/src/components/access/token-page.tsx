import { useEffect, useState } from "react";
import { accessApi, describeRule, productName, RISKY, type Approval, type AuditEvent, type Grant } from "@/lib/access-api";
import { loadInventory, resourcesByProduct } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";
import { Link } from "../link";
import { Connecting, ErrorNote, Panel } from "../ui";
import { GrantRow } from "./overview";
import { AccessShell, relative } from "./shell";
import { usePoll } from "./use-access";

/**
 * `/dashboard/access/tokens/$id`: one token in one place, as GitHub's and Cloudflare's token pages:
 * what it may do in words (resources by name), what it's allowed for a while, the approvals it
 * asked for, and its recent requests; edit, rotate and revoke at the top.
 */
export function TokenPage({ id }: { id: string }) {
  const { data: g, error, refresh } = usePoll(() => accessApi.token(id), 10_000);
  const { data: events } = usePoll(() => accessApi.audit({ grant: id, limit: 30 }), 15_000);
  const { data: approvals } = usePoll(() => accessApi.approvals().then((l) => l.filter((a) => a.grant_id === id)), 15_000);
  const ws = useWorkspace();
  const [inventory, setInventory] = useState<Record<string, { id: string; name: string }[]>>({});
  useEffect(() => {
    loadInventory()
      .then((inv) => setInventory(resourcesByProduct(inv)))
      .catch(() => {});
  }, []);
  const projectName = (p: string) => ws.projects.find((x) => x.id === p)?.name ?? p;
  const resourceName = (product: string, r: string) => inventory[product]?.find((x) => x.id === r)?.name ?? r;

  return (
    <AccessShell tab="overview">
      <div className="space-y-6">
        <Link href="/dashboard/access" className="text-[12px] text-muted hover:text-fg">
          ← Tokens and connections
        </Link>
        {!g ? (
          error ? <ErrorNote error={error} /> : <Connecting />
        ) : (
          <>
            <Panel title="Token">
              <ul>
                <GrantRow g={g} refresh={refresh} linked={false} />
              </ul>
            </Panel>
            <Panel title="What it may do">
              <div className="space-y-1 p-5 text-[13px]">
                {g.rules.map((r, i) => (
                  <p key={i} className="text-muted">
                    {describeRule(r, projectName, resourceName)}
                  </p>
                ))}
                <p className="text-[12px] text-subtle">{g.ask_otherwise ? "Anything else waits for your approval." : "Anything else is refused."}</p>
              </div>
            </Panel>
            <Temporary g={g} projectName={projectName} resourceName={resourceName} />
            <Approvals list={approvals ?? []} />
            <Requests id={id} events={events} />
          </>
        )}
      </div>
    </AccessShell>
  );
}

/** What remembered approvals allow it for now, until when. */
function Temporary({ g, projectName, resourceName }: { g: Grant; projectName: (id: string) => string; resourceName: (p: string, id: string) => string }) {
  const live = (g.temporary ?? []).filter((t) => t.until > Date.now());
  if (live.length === 0) return null;
  return (
    <Panel title="Allowed for a while">
      <ul className="divide-y divide-line">
        {live.map((t, i) => (
          <li key={i} className="flex flex-wrap items-center gap-3 px-5 py-2.5 text-[12px]">
            <span className="min-w-0 flex-1 text-muted">{describeRule(t.rule, projectName, resourceName)}</span>
            <span className="shrink-0 text-subtle">until {relative(t.until)}</span>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

function Approvals({ list }: { list: Approval[] }) {
  if (list.length === 0) return null;
  return (
    <Panel title="Approvals it asked for">
      <ul className="divide-y divide-line">
        {list.slice(0, 20).map((a) => (
          <li key={a.id} className="flex items-center gap-3 px-5 py-2.5 text-[12px]">
            <span className={`w-16 shrink-0 ${a.state === "pending" ? "text-accent" : a.state === "denied" ? "text-danger" : "text-subtle"}`}>{a.state}</span>
            <span className="min-w-0 flex-1 truncate text-muted">
              <b className={`font-medium ${RISKY.includes(a.action) ? "text-danger" : "text-fg"}`}>{a.action}</b> {productName(a.product)} {a.resource ?? ""}
            </span>
            <Link href={`/dashboard/access/approvals?id=${encodeURIComponent(a.id)}`} className="shrink-0 text-accent hover:underline">
              {a.state === "pending" ? "Decide" : relative(a.decided_at ?? a.created_at)}
            </Link>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

function Requests({ id, events }: { id: string; events: AuditEvent[] | null }) {
  return (
    <Panel title="Recent requests">
      {events === null ? (
        <div className="p-5">
          <Connecting />
        </div>
      ) : events.length === 0 ? (
        <p className="p-5 text-[12px] text-subtle">None yet.</p>
      ) : (
        <ul className="divide-y divide-line">
          {events.map((e) => (
            <li key={e.id} className="flex flex-wrap items-center gap-3 px-5 py-2 font-mono text-[11px]">
              <span className={`w-12 shrink-0 ${e.decision === "deny" ? "text-danger" : e.decision === "ask" ? "text-accent" : "text-subtle"}`}>{e.decision}</span>
              <span className="min-w-0 flex-1 truncate text-muted">
                {e.tool ?? `${e.method} ${e.route}`}
                {e.resource ? ` · ${e.resource}` : ""}
              </span>
              <span className="shrink-0 text-subtle">
                {e.status} · {relative(e.at)}
              </span>
            </li>
          ))}
          <li className="px-5 py-2 text-[12px]">
            <Link href={`/dashboard/access/audit?grant=${encodeURIComponent(id)}`} className="text-accent hover:underline">
              All of its requests
            </Link>
          </li>
        </ul>
      )}
    </Panel>
  );
}
