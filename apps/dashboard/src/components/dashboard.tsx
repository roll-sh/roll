"use client";

import { DotField } from "@roll/brand";
import { useRef, useState } from "react";
import { AppShell } from "./app-shell";
import { useMarkState } from "./mark-state";

const stats = [
  { label: "Requests · 24h", value: "18.2M", delta: "+12%" },
  { label: "p95 latency", value: "118ms", delta: "−81%" },
  { label: "Error rate", value: "0.02%", delta: "−0.01" },
  { label: "Active regions", value: "32", delta: "+4" },
];

type Status = "building" | "ready" | "failed";
interface Deployment {
  id: string;
  commit: string;
  branch: string;
  status: Status;
  age: string;
}

const seed: Deployment[] = [
  { id: "dpl_8f2a", commit: "Cache auth tokens at the edge", branch: "main", status: "ready", age: "2h" },
  { id: "dpl_7c91", commit: "Right-size checkout memory to 256MB", branch: "main", status: "ready", age: "1d" },
  { id: "dpl_6b40", commit: "Add eu-north-1 region", branch: "regions", status: "failed", age: "2d" },
];

const commits = [
  "Stream responses from /api/search",
  "Pin checkout to 6 edge regions",
  "Upgrade runtime to node 26",
  "Batch webhook retries",
];

const DEPLOY_MS = 2600;

function StatusDot({ status }: { status: Status }) {
  const cls = status === "ready" ? "bg-ok" : status === "failed" ? "bg-danger" : "bg-accent animate-pulse";
  return <span className={`size-2 rounded-full ${cls}`} />;
}

export function Dashboard() {
  const { state, play } = useMarkState();
  const [deployments, setDeployments] = useState(seed);
  const counter = useRef(0);
  const deploying = deployments.some((d) => d.status === "building");

  const deploy = () => {
    const n = counter.current++;
    const id = `dpl_${(0x9a00 + n * 0x1f3).toString(16)}`;
    setDeployments((ds) => [
      { id, commit: commits[n % commits.length], branch: "main", status: "building", age: "now" },
      ...ds,
    ]);
    play([
      ["deploying", DEPLOY_MS],
      ["success", 1800],
    ]);
    setTimeout(() => setDeployments((ds) => ds.map((d) => (d.id === id ? { ...d, status: "ready" } : d))), DEPLOY_MS);
  };

  return (
    <AppShell
      crumb="production"
      actions={
        <button
          onClick={deploy}
          disabled={deploying}
          className="shrink-0 rounded-lg bg-fg px-3 py-1.5 text-[13px] font-medium text-bg transition hover:opacity-85 disabled:opacity-50"
        >
          {deploying ? "Deploying…" : "Deploy"}
        </button>
      }
    >
      <h1 className="text-2xl font-semibold tracking-[-0.04em]">Overview</h1>

      <div className="mt-6 grid grid-cols-2 gap-px overflow-hidden rounded-2xl border border-line bg-line lg:grid-cols-4">
        {stats.map((s) => (
          <div key={s.label} className="bg-panel p-5">
            <p className="text-[12px] text-subtle">{s.label}</p>
            <p className="mt-2 text-2xl font-semibold tracking-[-0.03em] tabular-nums">{s.value}</p>
            <p className="mt-1 text-[12px] text-ok">{s.delta}</p>
          </div>
        ))}
      </div>

      <div className="mt-4 grid gap-4 lg:grid-cols-5">
        <section className="relative isolate h-72 overflow-hidden rounded-2xl border border-line bg-panel lg:col-span-3">
          <DotField
            spacing={18}
            radius={90}
            mode={state === "thinking" || state === "deploying" ? "thinking" : "idle"}
            className="-z-10 text-fg"
          />
          <div className="flex h-full flex-col justify-between p-5">
            <div>
              <h2 className="text-[15px] font-medium">Edge network</h2>
              <p className="text-[13px] text-muted">32 regions · all healthy</p>
            </div>
            <p className="font-mono text-[11px] text-subtle">
              {state === "deploying" ? "propagating to regions…" : "hover the network"}
            </p>
          </div>
        </section>

        <section className="rounded-2xl border border-line bg-panel lg:col-span-2">
          <h2 className="border-b border-line px-5 py-4 text-[15px] font-medium">Deployments</h2>
          <ul>
            {deployments.slice(0, 5).map((d) => (
              <li key={d.id} className="rise flex items-center gap-3 border-b border-line px-5 py-3 last:border-0">
                <StatusDot status={d.status} />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-[13px]">{d.commit}</p>
                  <p className="font-mono text-[11px] text-subtle">
                    {d.id} · {d.branch}
                  </p>
                </div>
                <span className="text-[12px] text-subtle capitalize">
                  {d.status === "building" ? <span className="shimmer">building</span> : d.age}
                </span>
              </li>
            ))}
          </ul>
        </section>
      </div>
    </AppShell>
  );
}
