import { KeyRound, Plus } from "lucide-react";
import { useState } from "react";
import { DATA_ROLE, esJson, searchApi, type KeyAccess, type NewSearchKey, type SearchCluster, type SearchKeyInfo } from "@/lib/search-api";
import { Field, Segmented } from "../redis/shared";
import { btn, ErrorNote, input, Panel, primary, SecretField, useCopy } from "../ui";

const EXPIRY = [
  { label: "Never", value: 0 },
  { label: "1 day", value: 86_400 },
  { label: "30 days", value: 30 * 86_400 },
  { label: "90 days", value: 90 * 86_400 },
  { label: "1 year", value: 365 * 86_400 },
];

/** Who made a key, from the names roll gives the keys it makes for apps and roll secrets. */
function madeBy(name: string | null): string | null {
  if (name?.startsWith("link ")) return "an app's link";
  if (name?.startsWith("roll secrets")) return "roll secrets";
  return null;
}

function day(ms: number) {
  return new Date(ms).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" });
}

function accessOf(k: SearchKeyInfo) {
  const roles = Object.keys(k.role_descriptors ?? {});
  if (roles.includes(DATA_ROLE)) return "Data";
  return roles.length ? "Limited" : "Admin";
}

/**
 * API keys of a cluster: a client that mustn't share the cluster's password gets a key of its
 * own, revoked without touching anyone else. The list is loaded on request (it wakes the
 * cluster); a new key is shown once. On an OpenSearch-flavor cluster (whose clients sign in with
 * a user) a key is an internal user of its own (`roll-key-*`).
 */
export function ApiKeys({ cluster }: { cluster: SearchCluster }) {
  const { copied, copy } = useCopy();
  const [keys, setKeys] = useState<SearchKeyInfo[] | null>(null);
  const [name, setName] = useState("");
  const [access, setAccess] = useState<KeyAccess>("data");
  const [expiry, setExpiry] = useState(0);
  const [made, setMade] = useState<NewSearchKey | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const opensearch = cluster.flavor === "opensearch";

  const act = async <T,>(label: string, f: () => Promise<T>): Promise<{ value: T } | null> => {
    setBusy(label);
    setError(null);
    try {
      return { value: await f() };
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setBusy(null);
    }
  };
  const load = () =>
    act("load", async () => {
      if (opensearch) {
        // The keys route's internal users: a data key has a role of its own, an admin key all_access.
        const users = await esJson<Record<string, { description?: string; opendistro_security_roles?: string[]; attributes?: Record<string, string> }>>(
          cluster.id,
          "GET",
          "/_plugins/_security/api/internalusers",
        );
        setKeys(
          Object.entries(users)
            .filter(([id]) => id.startsWith("roll-key-"))
            .map(([id, u]) => ({
              id,
              name: (u.description ?? "").replace(/^roll key: /, ""),
              creation: 0,
              expiration: Number(u.attributes?.roll_expires_at) || undefined,
              invalidated: false,
              role_descriptors: u.opendistro_security_roles?.includes(id) ? { [DATA_ROLE]: {} } : {},
            })),
        );
        return;
      }
      const r = await esJson<{ api_keys: SearchKeyInfo[] }>(cluster.id, "GET", "/_security/api_key");
      setKeys(r.api_keys.filter((k) => !k.invalidated).sort((a, b) => b.creation - a.creation));
    });

  return (
    <Panel
      title="API keys"
      aside={
        <button onClick={load} disabled={busy !== null} className={btn} title="Lists the cluster's keys (wakes it)">
          {busy === "load" ? <span className="shimmer">Loading…</span> : keys ? "Refresh" : "Show keys"}
        </button>
      }
    >
      <div className="space-y-4 px-5 py-4">
        <p className="text-[13px] text-muted">
          A client that shouldn&apos;t share the cluster&apos;s password gets a key of its own, sent as{" "}
          <span className="font-mono">{opensearch ? "a user name and password (Basic auth)" : "Authorization: ApiKey <key>"}</span>. Data keys read and write indices, templates, pipelines and
          lifecycle policies, but can&apos;t manage users or keys.
        </p>
        <form
          className="grid items-end gap-3 sm:grid-cols-[minmax(0,1fr)_auto_auto_auto]"
          onSubmit={async (e) => {
            e.preventDefault();
            const r = await act("create", () => searchApi.createKey(cluster.id, name.trim(), access, expiry || undefined));
            if (r) {
              setMade(r.value);
              setName("");
              if (keys) await load();
            }
          }}
        >
          <Field label="Name">
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="ingest worker" className={`${input} w-full`} maxLength={1024} />
          </Field>
          <Field label="Access">
            <Segmented
              value={access}
              options={[
                { label: "Data", value: "data" },
                { label: "Admin", value: "admin" },
              ]}
              onChange={setAccess}
            />
          </Field>
          <Field label="Expires">
            <select value={expiry} onChange={(e) => setExpiry(Number(e.target.value))} className={input}>
              {EXPIRY.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </select>
          </Field>
          <button type="submit" disabled={busy !== null || !name.trim()} className={primary}>
            {busy === "create" ? (
              <span className="shimmer">Creating…</span>
            ) : (
              <>
                <Plus className="size-3.5" /> Create key
              </>
            )}
          </button>
        </form>
        <ErrorNote error={error} />

        {made && (
          <div className="rise space-y-3 rounded-lg border border-line bg-bg p-4">
            <div className="flex items-center justify-between gap-3">
              <span className="flex items-center gap-2 text-[13px] font-medium">
                <KeyRound className="size-4 text-accent" /> {made.name}
              </span>
              <span className="text-[12px] text-subtle">shown once</span>
            </div>
            {made.api_key ? (
              <SecretField id="key" value={made.api_key} masked="••••••••••••••••••••••••" copied={copied} onCopy={copy} />
            ) : (
              <div className="grid gap-2 sm:grid-cols-2">
                <SecretField id="user" value={made.username ?? ""} masked={made.username ?? ""} copied={copied} onCopy={copy} />
                <SecretField id="pw" value={made.password ?? ""} masked="••••••••••••••••••••••••" copied={copied} onCopy={copy} />
              </div>
            )}
            <div className="flex flex-wrap items-center gap-2">
              <button
                onClick={() =>
                  copy(
                    "env",
                    made.api_key
                      ? `ELASTICSEARCH_URL=${made.url}\nELASTICSEARCH_API_KEY=${made.api_key}\n`
                      : `OPENSEARCH_URL=${made.url}\nOPENSEARCH_USERNAME=${made.username}\nOPENSEARCH_PASSWORD=${made.password}\n`,
                  )
                }
                className={btn}
              >
                {copied === "env" ? "Copied .env" : "Copy as .env"}
              </button>
              {made.expires_at && <span className="text-[12px] text-muted">expires {day(made.expires_at * 1000)}</span>}
              <button onClick={() => setMade(null)} className={`${primary} ml-auto`}>
                I&apos;ve saved it
              </button>
            </div>
          </div>
        )}

        {keys && (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full text-[13px]">
              <thead className="text-left text-[11px] text-subtle">
                <tr className="border-b border-line">
                  <th className="px-3 py-2 font-normal">Name</th>
                  <th className="px-3 py-2 font-normal">Access</th>
                  <th className="px-3 py-2 font-normal">Created</th>
                  <th className="px-3 py-2 font-normal">Expires</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {keys.length === 0 && (
                  <tr>
                    <td colSpan={5} className="px-3 py-3 text-muted">
                      No keys yet.
                    </td>
                  </tr>
                )}
                {keys.map((k) => {
                  const by = madeBy(k.name);
                  return (
                    <tr key={k.id} className="border-b border-line last:border-0">
                      <td className="max-w-72 px-3 py-2">
                        <div className="truncate">{k.name ?? <span className="text-subtle">unnamed</span>}</div>
                        <div className="truncate font-mono text-[11px] text-subtle">
                          {k.id}
                          {by && ` · made by ${by}`}
                        </div>
                      </td>
                      <td className="px-3 py-2 text-muted">{accessOf(k)}</td>
                      <td className="px-3 py-2 font-mono text-[12px] text-muted">{k.creation ? day(k.creation) : "—"}</td>
                      <td className="px-3 py-2 font-mono text-[12px] text-muted">{k.expiration ? day(k.expiration) : "never"}</td>
                      <td className="whitespace-nowrap px-3 py-2 text-right">
                        {revoking === k.id ? (
                          <span className="inline-flex items-center gap-2">
                            <span className="text-[12px] text-muted">{by ? `Cuts off ${by}.` : "Clients using it fail at once."}</span>
                            <button
                              disabled={busy !== null}
                              onClick={async () => {
                                const r = await act("revoke", () => searchApi.revokeKey(cluster.id, k.id));
                                setRevoking(null);
                                if (r) await load();
                              }}
                              className={`${btn} hover:border-danger/50 hover:text-danger`}
                            >
                              {busy === "revoke" ? <span className="shimmer">Revoking…</span> : "Revoke"}
                            </button>
                            <button onClick={() => setRevoking(null)} className={btn}>
                              Cancel
                            </button>
                          </span>
                        ) : (
                          <button onClick={() => setRevoking(k.id)} className={btn}>
                            Revoke
                          </button>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </Panel>
  );
}
