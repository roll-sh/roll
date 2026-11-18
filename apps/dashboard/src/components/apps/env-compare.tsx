import { ArrowLeftRight } from "lucide-react";
import { useState, type ReactNode } from "react";
import { appsApi, type Environment, type EnvironmentDiff, type Service } from "@/lib/apps-api";
import { btn, input } from "../ui";
import { type Runner } from "./shared";

/** Compare this environment with another (Railway's environment sync): the variables and
 *  settings that differ, never a value, each made the other's through the usual routes. */
export function CompareSection({ service, env, runner }: { service: Service; env: Environment; runner: Runner }) {
  const others = service.environments.filter((e) => e.name !== env.name);
  const [from, setFrom] = useState(others[0]?.name ?? "");
  const [diff, setDiff] = useState<EnvironmentDiff | null>(null);
  if (!others.length) return null;
  const busy = runner.busy !== null;
  const look = (f = from) => runner.run("compare", async () => setDiff(await appsApi.diff(service.id, env.name, f)));
  const copy = (names: string[]) =>
    runner.run("compare-copy", async () => {
      await appsApi.setVariables(service.id, env.name, { copy_from: from, copy: names });
      setDiff(await appsApi.diff(service.id, env.name, from));
    });
  const use = (puts: EnvironmentDiff["settings"]) =>
    runner.run("compare-use", async () => {
      for (const s of puts) await appsApi.putEnvironment(service.id, env.name, s.put);
      setDiff(await appsApi.diff(service.id, env.name, from));
    });
  const copyable = diff ? [...diff.variables.missing.filter((v) => !v.sealed).map((v) => v.name), ...(diff.variables.differ ?? [])] : [];
  const same = diff && !diff.variables.missing.length && !diff.variables.extra.length && !diff.variables.differ?.length && !diff.settings.length;
  return (
    <div className="space-y-3 px-5 py-4">
      <div className="flex flex-wrap items-end gap-3">
        <div className="min-w-56 flex-1">
          <p className="text-[13px] font-medium">Compare</p>
          <p className="mt-1 text-[12px] text-muted">The variables and settings {env.name} doesn't share with another environment, each copied here on its own. Values are never shown.</p>
        </div>
        <select
          value={from}
          onChange={(e) => {
            setFrom(e.target.value);
            setDiff(null);
          }}
          className={`${input} font-mono text-[12px]`}
          aria-label="Environment to compare with"
        >
          {others.map((e) => (
            <option key={e.name} value={e.name}>
              {e.name}
            </option>
          ))}
        </select>
        <button className={btn} disabled={!from || busy} onClick={() => void look()}>
          <ArrowLeftRight className="size-3.5" /> {runner.busy === "compare" ? <span className="shimmer">Comparing…</span> : `Compare with ${from}`}
        </button>
      </div>
      {diff && (
        <div className="rise space-y-3 rounded-lg border border-line bg-bg px-3 py-3 text-[13px]">
          {same ? (
            <p className="text-muted">
              {env.name} has {diff.from}'s variables and settings{diff.variables.differ === null ? " (values not compared: that needs reveal on both)" : ""}.
            </p>
          ) : (
            <>
              {(diff.variables.missing.length > 0 || (diff.variables.differ?.length ?? 0) > 0 || diff.variables.extra.length > 0) && (
                <div className="space-y-1.5">
                  <div className="flex items-center justify-between gap-3">
                    <p className="text-[11px] font-medium uppercase tracking-[0.06em] text-subtle">Variables</p>
                    {copyable.length > 1 && (
                      <button type="button" className={btn} disabled={busy} onClick={() => void copy(copyable)}>
                        Copy {copyable.length} from {diff.from}
                      </button>
                    )}
                  </div>
                  {diff.variables.missing.map((v) => (
                    <Row key={`m:${v.name}`} name={v.name} what={v.sealed ? `sealed in ${diff.from}: set it here by hand` : `only in ${diff.from}`}>
                      {!v.sealed && <Small onClick={() => void copy([v.name])} disabled={busy} label="Copy" />}
                    </Row>
                  ))}
                  {(diff.variables.differ ?? []).map((n) => (
                    <Row key={`d:${n}`} name={n} what="another value here">
                      <Small onClick={() => void copy([n])} disabled={busy} label={`Use ${diff.from}'s`} />
                    </Row>
                  ))}
                  {diff.variables.extra.map((n) => (
                    <Row key={`e:${n}`} name={n} what={`only in ${env.name}`} />
                  ))}
                  {diff.variables.differ === null && <p className="text-[12px] text-subtle">Values aren't compared: that needs reveal on both environments.</p>}
                </div>
              )}
              {diff.settings.length > 0 && (
                <div className="space-y-1.5">
                  <div className="flex items-center justify-between gap-3">
                    <p className="text-[11px] font-medium uppercase tracking-[0.06em] text-subtle">Settings</p>
                    {diff.settings.length > 1 && (
                      <button type="button" className={btn} disabled={busy} onClick={() => void use(diff.settings)}>
                        Use all {diff.from}'s
                      </button>
                    )}
                  </div>
                  {diff.settings.map((s) => (
                    <Row key={s.field} name={s.field} what={`${show(s.to)} here, ${show(s.from)} in ${diff.from}`}>
                      <Small onClick={() => void use([s])} disabled={busy} label={`Use ${diff.from}'s`} />
                    </Row>
                  ))}
                </div>
              )}
            </>
          )}
        </div>
      )}
    </div>
  );
}

function Row({ name, what, children }: { name: string; what: string; children?: ReactNode }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-2">
      <span className="min-w-0">
        <span className="font-mono text-[12px] text-fg">{name}</span> <span className="text-[12px] text-muted">{what}</span>
      </span>
      {children}
    </div>
  );
}

function Small({ label, onClick, disabled }: { label: string; onClick: () => void; disabled: boolean }) {
  return (
    <button type="button" className="text-[12px] text-accent hover:underline disabled:opacity-50" disabled={disabled} onClick={onClick}>
      {label}
    </button>
  );
}

/** A setting's value, short: lists by their length, objects and pages by whether they're set. */
function show(v: unknown): string {
  if (v === null || v === undefined || v === false) return "none";
  if (v === true) return "on";
  if (Array.isArray(v)) return v.length ? `${v.length} ${v.length === 1 ? "rule" : "rules"}` : "none";
  if (typeof v === "object") return Object.entries(v as Record<string, unknown>).map(([k, x]) => `${k} ${typeof x === "object" ? JSON.stringify(x) : String(x)}`).join(", ");
  return String(v);
}
