import { useEffect, useState } from "react";
import { appsApi, type SpendLimits } from "@/lib/apps-api";
import { Field } from "../redis/shared";
import { input, Panel, primary } from "../ui";
import type { Runner } from "./shared";

const dollars = (v: string) => (v.trim() === "" ? null : Number(v));
const shown = (v: number | null) => (v == null ? "" : String(v));

/** A project's spend limits: an alert (a notification) and a hard limit that pauses it. */
export function SpendPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [limits, setLimits] = useState<SpendLimits | null>(null);
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
    </Panel>
  );
}
