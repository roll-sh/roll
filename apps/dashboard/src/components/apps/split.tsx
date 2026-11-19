import { useState } from "react";
import { appsApi, type Environment } from "@/lib/apps-api";
import { input, Panel, primary } from "../ui";
import { SureButton, type Runner } from "./shared";

/**
 * An environment's split test (Netlify's): a share of its visitors, each kept by their bucket
 * cookie, served by another environment of the service.
 */
export function SplitPanel({ service, env, others, runner }: { service: string; env: Environment; others: string[]; runner: Runner }) {
  const split = env.split;
  const [to, setTo] = useState(others[0] ?? "");
  const [percent, setPercent] = useState("10");
  const busy = runner.busy !== null;
  const n = Number(percent);
  const save = (next: { environment: string; percent: number } | null) => runner.run("split", () => appsApi.putEnvironment(service, env.name, { split: next }));
  if (!split && others.length === 0) return null;
  return (
    <Panel title="Split test" aside={<span className="text-[12px] text-muted">a visitor stays on one side by their cookie</span>}>
      {split ? (
        <div className="flex flex-wrap items-center gap-3 px-5 py-4 text-[12.5px]">
          <span className="min-w-0 flex-1">
            <span className="font-mono">{split.percent}%</span> of visitors are served by <span className="font-medium">{split.environment}</span>, the rest by {env.name}.
          </span>
          <SureButton className="text-[12px] text-muted hover:text-fg" label="End the split test" sure="Sure? End it" disabled={busy} onSure={() => save(null)}>
            End
          </SureButton>
        </div>
      ) : (
        <form
          className="flex flex-wrap items-center gap-2 px-5 py-4 text-[12.5px]"
          onSubmit={(e) => {
            e.preventDefault();
            void save({ environment: to, percent: n });
          }}
        >
          <input value={percent} onChange={(e) => setPercent(e.target.value.replace(/\D/g, ""))} aria-label="Percent" className={`${input} w-16 font-mono text-[12px]`} />
          <span className="text-muted">% of visitors to</span>
          <select value={to} onChange={(e) => setTo(e.target.value)} aria-label="Environment" className={`${input} w-40 text-[12px]`}>
            {others.map((o) => (
              <option key={o} value={o}>
                {o}
              </option>
            ))}
          </select>
          <button type="submit" className={primary} disabled={busy || !to || !(n >= 1 && n <= 99)}>
            Start
          </button>
        </form>
      )}
    </Panel>
  );
}
