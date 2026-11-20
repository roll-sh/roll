import { useEffect, useState } from "react";
import { appsApi, type ProjectUsage, type SpendLimits } from "@/lib/apps-api";
import { Field } from "../redis/shared";
import { input, Panel, primary } from "../ui";
import type { Runner } from "./shared";

const dollars = (v: string) => (v.trim() === "" ? null : Number(v));
const shown = (v: number | null) => (v == null ? "" : String(v));

/** A project's spend limits: an alert (a notification) and a hard limit that pauses it. */
export function SpendPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [limits, setLimits] = useState<SpendLimits | null>(null);
  const [usage, setUsage] = useState<ProjectUsage | null>(null);
  useEffect(() => {
    appsApi.projectUsage(project).then(setUsage, () => setUsage(null));
  }, [project]);
  const [alert, setAlert] = useState("");
  const [hard, setHard] = useState("");
  useEffect(() => {
    appsApi.limits(project).then((l) => {
      setLimits(l);
      setAlert(shown(l.alert_usd));
      setHard(shown(l.hard_usd));
    }, () => setLimits(null));
  }, [project]);
  const [a, h] = [dollars(alert), dollars(hard)];
  const valid = [a, h].every((x) => x === null || (Number.isFinite(x) && x > 0)) && !(a !== null && h !== null && a > h);
  const changed = limits !== null && (a !== limits.alert_usd || h !== limits.hard_usd);

  return (
    <Panel
      title="Spend limits"
      aside={
        limits && (
          <span className={`font-mono text-[12px] ${limits.capped ? "text-danger" : "text-muted"}`}>
            ${limits.month_usd.toFixed(2)} this month{limits.capped ? " · paused" : ""}
          </span>
        )
      }
    >
      <form
        className="space-y-3 p-5"
        onSubmit={(e) => {
          e.preventDefault();
          void runner.run("limits", () => appsApi.setLimits(project, { alert_usd: a, hard_usd: h }).then(setLimits));
        }}
      >
        <p className="text-[13px] text-muted">
          For every service in <span className="text-fg">{projectName}</span>, this calendar month (UTC). Past the alert, the project's notification targets hear of it. At the limit its services stop and
          visitors get a "paused" page until you raise the limit or the month ends.
        </p>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Alert at ($)" hint="none: no alert">
            <input value={alert} onChange={(e) => setAlert(e.target.value.replace(/[^\d.]/g, ""))} placeholder="20" inputMode="decimal" className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Hard limit ($)" hint="none: never paused">
            <input value={hard} onChange={(e) => setHard(e.target.value.replace(/[^\d.]/g, ""))} placeholder="50" inputMode="decimal" className={`${input} w-full font-mono text-[12px]`} />
          </Field>
        </div>
        <div className="flex items-center justify-between gap-3">
          <span className="text-[12px] text-subtle">{valid ? "" : "The alert is at most the limit, and both are more than $0."}</span>
          <button type="submit" className={primary} disabled={!changed || !valid || runner.busy !== null}>
            {runner.busy === "limits" ? <span className="shimmer">Saving…</span> : "Save"}
          </button>
        </div>
      </form>
      {usage && usage.rows.length > 0 && (
        <div className="border-t border-line px-5 py-4">
          <p className="mb-2 flex items-baseline justify-between text-[12px] text-muted">
            <span>What it costs this month, by service</span>
            {usage.projected_usd != null && <span className="font-mono">≈ ${usage.projected_usd.toFixed(2)} by the month's end</span>}
          </p>
          <table className="w-full text-[12.5px]" aria-label="Cost by service">
            <tbody>
              {usage.rows.map((r) => (
                <tr key={`${r.service ?? "deleted"}/${r.environment ?? (r.previews ? "previews" : "")}`} className="border-b border-line last:border-0">
                  <td className="py-1.5">
                    {r.deleted ? <span className="text-subtle">Deleted services</span> : r.name}
                    {!r.deleted && <span className="ml-2 text-[11.5px] text-subtle">{r.previews ? "previews" : r.environment}</span>}
                  </td>
                  <td className="py-1.5 text-right font-mono">${r.usd.toFixed(2)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}
