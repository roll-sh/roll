import { useEffect, useState } from "react";
import type { AuditEntry, AuditFilter } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, ErrorNote, errorText, sized, ghost, stamp, Teach } from "./ui";

const PAGE = 100;
const ALL = "*";

/** Every reveal, export, resolve and write, newest first (values never appear here). */
export function AuditView() {
  const { api, envs, project } = useSecrets();
  const [env, setEnv] = useState(ALL);
  const [key, setKey] = useState("");
  const [actor, setActor] = useState("");
  const [action, setAction] = useState("");
  const [rows, setRows] = useState<AuditEntry[] | null>(null);
  const [end, setEnd] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const filter: AuditFilter = { env: env === ALL ? undefined : env, key: key.trim() || undefined, actor: actor.trim() || undefined, action: action.trim() || undefined, limit: PAGE };
  const sig = JSON.stringify(filter);

  // Filters typed into fields settle for a moment before they load.
  useEffect(() => {
    let live = true;
    const t = setTimeout(() => {
      api.audit(filter).then(
        (r) => {
          if (!live) return;
          setRows(r);
          setEnd(r.length < PAGE);
          setError(null);
        },
        (e) => live && setError(errorText(e)),
      );
    }, 200);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [api, sig]);

  const older = () => {
    const last = rows?.[rows.length - 1];
    if (!last) return;
    api.audit({ ...filter, before: last.id }).then(
      (r) => {
        setRows((x) => [...(x ?? []), ...r]);
        setEnd(r.length < PAGE);
      },
      (e) => setError(errorText(e)),
    );
  };

  return (
    <div className="mx-auto max-w-5xl px-4 py-5">
      <div className="flex flex-wrap items-center gap-2">
        <Select label="Environment" value={env} onChange={setEnv} options={[{ value: ALL, label: "All environments" }, ...envs.map((e) => ({ value: e.slug, label: e.name }))]} className="w-44" />
        <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="Key" aria-label="Key" className={`${sized} w-40 font-mono`} />
        <input value={actor} onChange={(e) => setActor(e.target.value)} placeholder="Actor (operator, token:rls_…)" aria-label="Actor" className={`${sized} w-56 font-mono`} />
        <input value={action} onChange={(e) => setAction(e.target.value)} placeholder="Action (secrets.reveal, token.)" aria-label="Action" className={`${sized} w-56 font-mono`} />
      </div>
      <ErrorNote error={error} className="mt-3" />
      {rows && rows.length === 0 ? (
        <Teach title="Nothing in the audit log matches" cli={[`roll secrets audit -p ${project.name} --key DATABASE_URL`]}>
          Every reveal, export, resolve and write is recorded here with who did it, from where and to which keys. Values never are. Rows are kept for 400 days.
        </Teach>
      ) : (
        <div className="mt-3 overflow-x-auto">
          <table className="w-full min-w-[720px] text-[12px]">
            <thead>
              <tr className="text-left text-[11px] text-subtle">
                <th className="py-1.5 pr-3 font-medium">When</th>
                <th className="py-1.5 pr-3 font-medium">Actor</th>
                <th className="py-1.5 pr-3 font-medium">Action</th>
                <th className="py-1.5 pr-3 font-medium">Where</th>
                <th className="py-1.5 font-medium">What</th>
              </tr>
            </thead>
            <tbody>
              {rows?.map((r) => (
                <tr key={r.id} className="border-t border-line align-top">
                  <td className="py-1.5 pr-3 whitespace-nowrap text-muted" title={stamp(r.at)}>
                    {ago(r.at)}
                  </td>
                  <td className="max-w-40 py-1.5 pr-3 text-soft">
                    <ActorMark who={r.actor} />
                  </td>
                  <td className="py-1.5 pr-3 font-mono text-[11.5px] whitespace-nowrap">{r.action}</td>
                  <td className="py-1.5 pr-3 font-mono text-[11px] whitespace-nowrap text-muted">
                    {r.environment ?? "—"}
                    {r.path && r.path !== "/" ? r.path : ""}
                  </td>
                  <td className="py-1.5">
                    <span className="text-soft">{r.summary}</span>
                    {r.keys.length > 0 && (
                      <span className="mt-0.5 block truncate font-mono text-[10.5px] text-subtle" title={r.keys.join(", ")}>
                        {r.keys.join(" ")}
                      </span>
                    )}
                    {r.ip && <span className="font-mono text-[10.5px] text-faint">{r.ip}</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {rows && !end && (
            <button onClick={older} className={`${ghost} mx-auto mt-3 flex`}>
              Older entries
            </button>
          )}
        </div>
      )}
    </div>
  );
}
