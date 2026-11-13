import { useEffect, useState } from "react";
import { accessApi, describeRule, productName, RISKY, statusOf, type Approval, type AuditEvent, type Grant, type Tightened } from "@/lib/access-api";
import { loadInventory, resourcesByProduct } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";
import { Link } from "../link";
import { btn, Connecting, ErrorNote, Panel, primary } from "../ui";
import { GrantRow } from "./overview";
import { AccessShell, relative } from "./shell";
import { usePoll } from "./use-access";

/**
 * `/dashboard/access/tokens/$id`: one token in one place, as GitHub's and Cloudflare's token pages:
 * what it may do in words (resources by name), what it's allowed for a while, the approvals it
 * asked for, and its recent requests; edit, rotate and revoke at the top.
 */
export function TokenPage({ id }: { id: string }) {
  // Another token's page starts from nothing, not from the last one's answers.
  return <Token key={id} id={id} />;
}

function Token({ id }: { id: string }) {
  const { data: g, error, refresh } = usePoll(() => accessApi.token(id), 10_000);
  const { data: events } = usePoll(() => accessApi.audit({ grant: id, limit: 30 }), 15_000);
  const { data: approvals } = usePoll(() => accessApi.approvals(undefined, id), 15_000);
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
        {g && error && <ErrorNote error={`Couldn't refresh this token (${error}): what's below may be out of date.`} />}
        {!g ? (
          error ? <ErrorNote error={error} /> : <Connecting />
        ) : (
          <>
            {statusOf(g) !== "active" && (
              <p className="rounded-lg border border-line px-4 py-3 text-[13px] text-muted">
                This token is {statusOf(g)}: nothing below works any more.
              </p>
            )}
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
            {statusOf(g) === "active" && <Temporary g={g} refresh={refresh} projectName={projectName} resourceName={resourceName} />}
            {statusOf(g) === "active" && <Tighten g={g} refresh={refresh} projectName={projectName} resourceName={resourceName} />}
            <Approvals list={approvals ?? []} />
            <Requests id={id} events={events} />
          </>
        )}
      </div>
    </AccessShell>
  );
}

/**
 * "Tighten to what it used": its rules narrowed to the actions and resources its allowed requests
 * used lately (the server never proposes anything wider), applied in one click.
 */
function Tighten({
  g,
  refresh,
  projectName,
  resourceName,
}: {
  g: Grant;
  refresh: () => Promise<void>;
  projectName: (id: string) => string;
  resourceName: (p: string, id: string) => string;
}) {
  const [t, setT] = useState<Tightened | null>(null);
  const [busy, setBusy] = useState(false);
  // A proposal is about the rules it was made from: once they change, it's gone.
  const rulesKey = JSON.stringify(g.rules);
  useEffect(() => setT(null), [rulesKey]);
  const [error, setError] = useState<string | null>(null);
  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const look = () => run(async () => setT(await accessApi.tighten(g.id)));
  const apply = () =>
    run(async () => {
      if (!t) return;
      await accessApi.applyTighten(g.id, t.from, t.days);
      setT(null);
      await refresh();
    });
  return (
    <Panel title="Tighten to what it used">
      <div className="space-y-3 p-5 text-[13px]">
        {!t ? (
          <div className="flex flex-wrap items-center gap-3">
            <p className="min-w-0 flex-1 text-muted">Narrow its rules to the actions and resources its requests used in the last 30 days.</p>
            <button onClick={look} disabled={busy} className={btn}>
              {busy ? <span className="shimmer">Reading its requests…</span> : "See what it used"}
            </button>
          </div>
        ) : !t.narrower ? (
          <p className="text-muted">
            {t.used.length === 0 ? "It made no allowed requests in the last 30 days: revoke it if nothing needs it." : "It used everything its rules allow: there's nothing to narrow."}
          </p>
        ) : (
          <>
            <p className="text-[12px] text-subtle">
              From {t.used.reduce((n, u) => n + u.count, 0)} requests over {t.days} days, it would only be able to:
            </p>
            <div className="space-y-1">
              {t.rules.map((r, i) => (
                <p key={i} className="text-muted">
                  {describeRule(r, projectName, resourceName)}
                </p>
              ))}
            </div>
            <div className="flex items-center gap-2">
              <button onClick={apply} disabled={busy} className={primary}>
                {busy ? <span className="shimmer">Applying…</span> : "Apply these rules"}
              </button>
              <button onClick={() => setT(null)} disabled={busy} className={btn}>
                Keep its rules
              </button>
            </div>
          </>
        )}
        <ErrorNote error={error} />
      </div>
    </Panel>
  );
}

/** What remembered approvals allow it for now, until when. */
function Temporary({
  g,
  refresh,
  projectName,
  resourceName,
}: {
  g: Grant;
  refresh: () => Promise<void>;
  projectName: (id: string) => string;
  resourceName: (p: string, id: string) => string;
}) {
  const [error, setError] = useState<string | null>(null);
  const live = (g.temporary ?? []).filter((t) => t.until > Date.now());
  if (live.length === 0) return null;
  const takeBack = async (approval: string) => {
    setError(null);
    try {
      await accessApi.forget(g.id, approval);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  return (
    <Panel title="Allowed for a while">
      <ul className="divide-y divide-line">
        {live.map((t, i) => (
          <li key={t.approval ?? i} className="flex flex-wrap items-center gap-3 px-5 py-2.5 text-[12px]">
            <span className="min-w-0 flex-1 text-muted">{describeRule(t.rule, projectName, resourceName)}</span>
            <span className="shrink-0 text-subtle">
              until {relative(t.until)}
              {t.approval && (
                <>
                  {" · "}
                  <Link href={`/dashboard/access/approvals?id=${encodeURIComponent(t.approval)}`} className="hover:text-fg hover:underline">
                    approved here
                  </Link>
                </>
              )}
            </span>
            {t.approval && (
              <button onClick={() => takeBack(t.approval!)} className={btn}>
                Take back
              </button>
            )}
          </li>
        ))}
      </ul>
      {error && (
        <div className="px-5 pb-3">
          <ErrorNote error={error} />
        </div>
      )}
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
    <Panel
      title="Recent requests"
      aside={events?.[0]?.client ? <span className="truncate text-[12px] text-subtle">Last from {events[0].client}</span> : undefined}
    >
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
