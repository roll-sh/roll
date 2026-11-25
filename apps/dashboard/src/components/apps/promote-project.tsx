import { ArrowRight } from "lucide-react";
import { useState } from "react";
import { appsApi, type Service } from "@/lib/apps-api";
import { btn, input, Panel } from "../ui";
import { since, type Runner } from "./shared";

type Row = { service: Service; deployment: string; revision: string | null; at: number };
type Done = Record<string, { ok: true; id: string } | { ok: false; error: string }>;

/** Promote a whole project (Heroku Pipelines' promote): what each service runs in one
 *  environment now, listed first, then put live in another, each through its own promote with
 *  that deployment as its `expect`, so nothing that changed since the list goes out. A service
 *  with nothing live there is said and left alone. `roll apps promote --all-services` does the same. */
export function PromotePanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [from, setFrom] = useState("staging");
  const [to, setTo] = useState("production");
  const [plan, setPlan] = useState<{ rows: Row[]; none: string[] } | null>(null);
  const [done, setDone] = useState<Done | null>(null);

  const check = () =>
    runner.run("promote:plan", async () => {
      setDone(null);
      const all = await appsApi.services(project);
      const rows: Row[] = [];
      const none: string[] = [];
      for (const s of all) {
        const live = s.environments.find((e) => e.name === from.trim())?.active;
        if (live) rows.push({ service: s, deployment: live.id, revision: live.revision, at: live.created_at });
        else none.push(s.name);
      }
      setPlan({ rows, none });
    });
  const promote = () =>
    runner.run("promote:go", async () => {
      if (!plan) return;
      const out: Done = {};
      for (const r of plan.rows) {
        try {
          const d = await appsApi.promote(r.service.id, to.trim(), from.trim(), r.deployment);
          out[r.service.id] = { ok: true, id: d.id };
        } catch (e) {
          out[r.service.id] = { ok: false, error: e instanceof Error ? e.message : String(e) };
        }
      }
      setDone(out);
    });
  const same = !from.trim() || !to.trim() || from.trim() === to.trim();

  return (
    <Panel title="Promote the project">
      <p className="px-5 pt-4 text-[13px] text-muted">
        What each of {projectName}'s services runs in one environment, put live in another: the same images, each with its own variables there. Nothing is built.
      </p>
      <div className="flex flex-wrap items-center gap-2 px-5 py-3">
        <input
          value={from}
          onChange={(e) => {
            setFrom(e.target.value);
            setPlan(null);
          }}
          className={`${input} w-32`}
          aria-label="From environment"
          spellCheck={false}
        />
        <ArrowRight className="size-3.5 text-subtle" strokeWidth={1.5} />
        <input
          value={to}
          onChange={(e) => {
            setTo(e.target.value);
            setPlan(null);
          }}
          className={`${input} w-32`}
          aria-label="To environment"
          spellCheck={false}
        />
        <button type="button" className={btn} disabled={same || runner.busy !== null} onClick={() => void check()}>
          {runner.busy === "promote:plan" ? <span className="shimmer">Reading…</span> : "Show what goes"}
        </button>
      </div>
      {plan && (
        <div className="px-5 pb-4">
          {plan.rows.length ? (
            <ul className="divide-y divide-line rounded-lg border border-line">
              {plan.rows.map((r) => {
                const d = done?.[r.service.id];
                return (
                  <li key={r.service.id} className="flex items-center gap-3 px-3 py-2 text-[13px]">
                    <span className="min-w-0 flex-1 truncate text-fg">{r.service.name}</span>
                    <span className="font-mono text-[12px] text-muted">{r.revision?.slice(0, 7) ?? r.deployment}</span>
                    <span className="text-[12px] text-subtle">{since(r.at)}</span>
                    {d && (d.ok ? <span className="text-[12px] text-accent">promoted</span> : <span className="max-w-[40%] truncate text-[12px] text-danger" title={d.error}>{d.error}</span>)}
                  </li>
                );
              })}
            </ul>
          ) : (
            <p className="text-[13px] text-muted">Nothing is live in {from.trim()} in this project.</p>
          )}
          {plan.none.length > 0 && <p className="mt-2 text-[12px] text-subtle">Left alone, nothing live in {from.trim()}: {plan.none.join(", ")}.</p>}
          {plan.rows.length > 0 && !done && (
            <button type="button" className={`${btn} mt-3`} disabled={runner.busy !== null} onClick={() => void promote()}>
              {runner.busy === "promote:go" ? <span className="shimmer">Promoting…</span> : `Promote ${plan.rows.length} ${plan.rows.length === 1 ? "service" : "services"} to ${to.trim()}`}
            </button>
          )}
        </div>
      )}
    </Panel>
  );
}
