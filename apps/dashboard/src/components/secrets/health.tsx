import { CircleCheck, Dices, FileSearch } from "lucide-react";
import { useState } from "react";
import { Link } from "@/components/link";
import { needsApproval, type Finding, type Op, type SecretsApi, type SetSecret } from "@/lib/secrets-api";
import { generateValue } from "@/lib/secrets-generate";
import { secretsHref, useSecrets } from "./secrets-page";
import { card, errorText, ErrorNote, ghost, Section, sized, useLoad } from "./ui";

const ISSUES: { issue: Finding["issue"]; title: string; what: string; serious: boolean }[] = [
  { issue: "expired", title: "Expired", what: "Their values stopped working outside roll.", serious: true },
  { issue: "missing", title: "Missing", what: "Required keys an environment doesn't define.", serious: true },
  { issue: "exposed", title: "Exposed", what: "A protected environment's value, also set where more people and less vetted code read it.", serious: true },
  { issue: "weak", title: "Weak", what: "Credentials easy to guess: New value replaces one with 48 random characters.", serious: false },
  { issue: "expiring", title: "Expiring", what: "Values that stop working outside roll within two weeks.", serious: false },
  { issue: "stale", title: "Stale", what: "Unchanged for longer than their environment allows.", serious: false },
  { issue: "unread", title: "Unread", what: "Nobody revealed, exported, resolved or synced them in 90 days: still needed?", serious: false },
  { issue: "unreferenced", title: "Not in the code", what: "No repository on this project's canvas mentions them (roll code's search): still needed?", serious: false },
];

/** Everything insights flag, in every environment at once, worst first (docs/secrets.md, "Health"). */
export function HealthView() {
  const { api, project, standard, reloadEnvs } = useSecrets();
  const [code, setCode] = useState(false);
  const base = useLoad(() => api.health(), [api]);
  // The code check apart: refused (it needs code read on every repository), the rest still shows.
  const coded = useLoad(() => (code ? api.health(true) : Promise.resolve(null)), [api, code]);
  const found = { data: coded.data ?? base.data, error: base.error, loading: code && coded.loading, reload: () => (base.reload(), code && coded.reload()) };
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const fix = async (f: Finding, body: SetSecret, what: string) => {
    setError(null);
    try {
      const asked = await write(api, f, body, what);
      setNote(asked ? `${f.key} in ${f.environment}: a change request is open (the environment is protected)` : `${f.key} in ${f.environment}: ${what}`);
      found.reload();
      reloadEnvs();
    } catch (e) {
      setError(errorText(e));
    }
  };
  const name = (slug: string) => standard.find((e) => e.slug === slug)?.name ?? slug;
  // Every weak key of one environment, a new random value each: one change, or one request.
  const fixAll = async (env: string, rows: Finding[]) => {
    setError(null);
    const ops: Op[] = rows.map((f) => ({ op: "set", key: f.key, path: f.path, value: generateValue() }));
    const message = `new random values for ${rows.length} weak ${rows.length === 1 ? "key" : "keys"} (from Health)`;
    try {
      try {
        await api.batch(env, { path: "/", message, ops });
        setNote(`${name(env)}: ${rows.length} weak ${rows.length === 1 ? "key has" : "keys have"} new values`);
      } catch (e) {
        if (!needsApproval(e)) throw e;
        await api.openRequest(env, { path: "/", message, ops });
        setNote(`${name(env)}: a change request is open for ${rows.length} new ${rows.length === 1 ? "value" : "values"} (the environment is protected)`);
      }
      found.reload();
      reloadEnvs();
    } catch (e) {
      setError(errorText(e));
    }
  };
  if (found.error) return <ErrorNote error={found.error} className="m-4" />;
  if (!found.data) return null;
  return (
    <div className="mx-auto max-w-4xl space-y-6 px-4 py-5">
      <div className="flex items-center justify-end">
        <button className={ghost} onClick={() => setCode(true)} disabled={code} title="Search the repositories on this project's canvas for each key (roll code's index)">
          <FileSearch className="size-3.5" /> {code ? (found.loading ? "Checking the code…" : "Code checked") : "Check the code"}
        </button>
      </div>
      <ErrorNote error={error ?? coded.error} onDismiss={() => setError(null)} />
      {note && (
        <p className="fade-in flex items-center gap-2 text-[12px] text-muted" role="status">
          <CircleCheck className="size-3.5 text-ok" /> {note}
        </p>
      )}
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
            {issue === "weak" && (
              <div className="mb-2 flex flex-wrap gap-1.5">
                {[...new Set(rows.map((f) => f.environment))].map((env) => {
                  const these = rows.filter((f) => f.environment === env);
                  return (
                    <button key={env} className={ghost} onClick={() => fixAll(env, these)} title="A random value for each, made here: one change, or one change request where protected">
                      <Dices className="size-3.5" /> New values for all {these.length} in {name(env)}
                    </button>
                  );
                })}
              </div>
            )}
            <div className={`${card} divide-y divide-line`}>
              {rows.map((f) => (
                <div key={`${f.environment}/${f.path}/${f.key}`} className="flex flex-wrap items-center gap-x-3 gap-y-0.5 px-3 py-1.5 text-[12.5px]">
                  <Link href={secretsHref(project.id, { env: f.environment, path: f.path === "/" ? undefined : f.path, key: f.issue === "missing" ? undefined : f.key })} className="flex min-w-0 items-center gap-3 rounded transition-colors hover:text-accent">
                    <span className="font-mono text-fg">{f.key}</span>
                    <span className="text-subtle">
                      {name(f.environment)}
                      {f.path !== "/" && <span className="font-mono"> {f.path}</span>}
                    </span>
                  </Link>
                  <span className={`ml-auto text-[11.5px] ${serious ? "text-danger" : "text-muted"}`}>{f.detail}</span>
                  {f.issue === "weak" && (
                    <button className={`${ghost} h-6`} onClick={() => fix(f, { value: generateValue() }, "a new random value")} title="48 random letters and digits, made here; a change request where the environment is protected">
                      <Dices className="size-3.5" /> New value
                    </button>
                  )}
                  {(f.issue === "expired" || f.issue === "expiring") && <NewDate onSet={(at) => fix(f, { expires_at: at }, "dated again")} />}
                </div>
              ))}
            </div>
          </Section>
        );
      })}
    </div>
  );
}

/** Set one finding's key (a change request where the environment is protected). True: one was opened. */
async function write(api: SecretsApi, f: Finding, body: SetSecret, what: string): Promise<boolean> {
  try {
    await api.set(f.environment, f.key, { ...body, message: `${what} (from Health)` }, f.path);
    return false;
  } catch (e) {
    if (!needsApproval(e)) throw e;
    await api.openRequest(f.environment, { path: f.path, message: `${what} (from Health)`, ops: [{ op: "set", key: f.key, ...body }] });
    return true;
  }
}

/** A new expiry date for a key whose value was renewed outside roll. */
function NewDate({ onSet }: { onSet: (at: number) => void }) {
  const [day, setDay] = useState("");
  return (
    <form
      className="flex items-center gap-1"
      onSubmit={(e) => {
        e.preventDefault();
        if (day) onSet(Date.parse(`${day}T00:00:00Z`));
      }}
    >
      <input type="date" value={day} onChange={(e) => setDay(e.target.value)} aria-label="New expiry date" className={`${sized} h-6 w-34 font-mono text-[11.5px]`} />
      <button type="submit" className={`${ghost} h-6`} disabled={!day}>
        Set
      </button>
    </form>
  );
}
