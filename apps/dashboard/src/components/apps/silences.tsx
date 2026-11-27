import { BellOff } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, type Silence } from "@/lib/apps-api";
import { btn, input } from "../ui";
import type { Runner } from "./shared";

const FOR = [
  { label: "30 minutes", ms: 30 * 60_000 },
  { label: "2 hours", ms: 2 * 3_600_000 },
  { label: "8 hours", ms: 8 * 3_600_000 },
  { label: "1 day", ms: 86_400_000 },
  { label: "1 week", ms: 7 * 86_400_000 },
];
const at = (ms: number) => new Date(ms).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });

/** An environment's alert silences (Cloud Monitoring's snooze): for a while its alerts reach no
 *  notification target, on-call included; events are still kept and resolves still go out. */
export function Silences({ service, env, runner }: { service: string; env: string; runner: Runner }) {
  const [open, setOpen] = useState<Silence[] | null>(null);
  const [ms, setMs] = useState(FOR[1]!.ms);
  const [reason, setReason] = useState("");
  const load = () => appsApi.silences(service, env).then(setOpen, () => setOpen([]));
  useEffect(() => {
    void load();
  }, [service, env]);
  if (open === null) return null;
  return (
    <div className="space-y-2 border-t border-line px-5 py-4 text-[12.5px]">
      <p className="flex items-center gap-1.5 text-muted">
        <BellOff className="size-3.5" strokeWidth={1.5} /> Silence its alerts for a while (a planned migration): no target is told, on-call included; Events still lists them.
      </p>
      {open.map((s) => (
        <div key={s.id} className="flex flex-wrap items-center gap-2">
          <span className="text-fg">Quiet until {at(s.until)}</span>
          <span className="text-subtle">{s.alerts.length ? s.alerts.join(", ") : "every alert"} · {s.reason}</span>
          <button type="button" className="text-[12px] text-accent hover:underline" disabled={runner.busy !== null} onClick={() => void runner.run(`silence:${s.id}`, async () => { await appsApi.endSilence(service, env, s.id); await load(); })}>
            End now
          </button>
        </div>
      ))}
      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          void runner.run("silence", async () => {
            await appsApi.addSilence(service, env, { until: Date.now() + ms, reason: reason.trim() });
            setReason("");
            await load();
          });
        }}
      >
        <select value={ms} onChange={(e) => setMs(Number(e.target.value))} aria-label="For" className={`${input} !w-auto text-[12px]`}>
          {FOR.map((f) => (
            <option key={f.ms} value={f.ms}>
              {f.label}
            </option>
          ))}
        </select>
        <input value={reason} onChange={(e) => setReason(e.target.value)} placeholder="Why (a database migration)" aria-label="Why" maxLength={200} className={`${input} min-w-56 flex-1 text-[12px]`} />
        <button type="submit" className={btn} disabled={runner.busy !== null || !reason.trim()}>
          Silence alerts
        </button>
      </form>
    </div>
  );
}
