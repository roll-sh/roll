import { useEffect, useState } from "react";
import { appsApi, type Prices, type Usage } from "@/lib/apps-api";
import { formatBytes } from "@/lib/roll-api";
import { monthOf, projectMonth, usd } from "@/lib/workspace-resources";
import { panel, Stats } from "../ui";
import { message } from "./shared";

const price = (n: number) => `$${n < 0.01 ? n.toFixed(6).replace(/0+$/, "") : n.toFixed(2)}`;
const compact = (n: number) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e4 ? `${(n / 1e3).toFixed(0)}k` : Math.round(n).toLocaleString("en-US"));

/** This month so far and where it's heading, what each thing costs, and that idle costs nothing. */
export function UsageTab({ service, env, prices }: { service: string; env: string; prices: Prices | null }) {
  const month = monthOf(new Date());
  const [u, setU] = useState<Usage | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    appsApi.usage(service, env, month.elapsed).then(
      (x) => live && setU(x),
      (e) => live && setError(message(e)),
    );
    return () => {
      live = false;
    };
  }, [service, env, month.elapsed]);

  const sum = (k: keyof Usage["days"][number]) => (u ? u.days.reduce((s, d) => s + d[k], 0) : 0);

  return (
    <div className="space-y-6">
      {error && <p className="text-[13px] text-danger">Usage didn't load: {error}</p>}
      <Stats
        items={[
          { label: `${month.label} so far`, value: u ? usd(u.total_usd) : "—", hint: `day ${month.elapsed} of ${month.days}` },
          { label: "Projected", value: u ? `≈ ${usd(projectMonth(u.total_usd, month))}` : "—", hint: `by ${month.end}, at this month's rate` },
          { label: "CPU", value: u ? `${compact(sum("cpu_seconds"))} s` : "—", hint: `${compact(sum("gb_seconds"))} GB-s memory` },
          { label: "Requests", value: u ? compact(sum("requests")) : "—", hint: `${formatBytes(sum("egress_bytes"))} out · ${Math.ceil(sum("build_seconds") / 60)} build min` },
        ]}
      />
      <div className="grid gap-4 lg:grid-cols-2">
        <section className={panel}>
          <div className="border-b border-line px-5 py-3 text-[15px] font-medium">Prices</div>
          <table className="w-full text-[13px]">
            <tbody>
              {(prices
                ? [
                    ["CPU", `${price(prices.cpu_second)} per vCPU-second used`],
                    ["Memory", `${price(prices.gb_second)} per GB-second resident`],
                    ["Egress", `${price(prices.egress_gb)} per GB`],
                    ["Builds", `${price(prices.build_minute)} per build minute`],
                    ["Requests", "free"],
                  ]
                : [["", "Loading…"]]
              ).map(([k, v]) => (
                <tr key={k} className="border-b border-line last:border-0">
                  <td className="px-5 py-2 text-muted">{k}</td>
                  <td className="px-5 py-2 text-right font-mono text-[12px]">{v}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="border-t border-line px-5 py-3 text-[12px] text-muted">
            Idle costs <span className="font-mono text-fg">$0.00</span>: a suspended or stopped instance meters nothing. Only seconds spent serving are billed.
          </p>
        </section>
        <section className={panel}>
          <div className="border-b border-line px-5 py-3 text-[15px] font-medium">By day</div>
          <div className="max-h-[260px] overflow-auto">
            <table className="w-full font-mono text-[12px] tabular-nums">
              <tbody>
                {u && u.days.every((d) => !d.usd && !d.requests && !d.cpu_seconds) && (
                  <tr>
                    <td className="px-5 py-3 font-sans text-[13px] text-muted">Nothing ran this month.</td>
                  </tr>
                )}
                {u?.days
                  .slice()
                  .reverse()
                  .map((d) => (
                    <tr key={d.day} className="border-b border-line last:border-0">
                      <td className="px-5 py-1.5 text-muted">{new Date(d.day * 86_400_000).toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: "UTC" })}</td>
                      <td className="px-3 py-1.5 text-right text-subtle">{compact(d.requests)} req</td>
                      <td className="px-3 py-1.5 text-right text-subtle">{compact(d.cpu_seconds)} cpu-s</td>
                      <td className="px-5 py-1.5 text-right">{usd(d.usd)}</td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>
        </section>
      </div>
    </div>
  );
}
