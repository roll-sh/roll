"use client";

import { DotField, RollMark, markStates, type MarkState } from "@roll/brand";
import { AskPanel } from "./ask";
import { useMarkState } from "./mark-state";

const sequences: { label: string; steps: [MarkState, number][] }[] = [
  { label: "Deploy", steps: [["deploying", 2600], ["success", 1800]] },
  { label: "Think → answer", steps: [["thinking", 2800], ["success", 1800]] },
  { label: "Fail", steps: [["deploying", 1600], ["error", 1800]] },
];

export function Hero() {
  const { state, setState, play } = useMarkState();

  return (
    <section className="relative isolate flex min-h-[calc(100svh-3.5rem)] flex-col items-center justify-center overflow-hidden px-4 py-16 text-center">
      <DotField mode={state === "thinking" ? "thinking" : "idle"} className="-z-10 text-fg" />
      <div className="pointer-events-none absolute inset-0 -z-10 bg-[radial-gradient(ellipse_at_center,transparent_0%,var(--bg)_72%)]" />

      <RollMark state={state} size={240} reach={1.2} className="text-fg" label={`roll mark, ${state}`} />
      <p className="mt-2 font-mono text-[11px] tracking-wide text-subtle uppercase">
        {state} · hover the nodes · click to ripple
      </p>

      <h1 className="mt-8 max-w-2xl text-4xl font-semibold tracking-[-0.045em] text-balance sm:text-6xl">
        Serverless infrastructure for everything.
      </h1>
      <p className="mt-4 max-w-md text-[15px] text-muted text-pretty">
        Every function, on every edge, optimized automatically. Ship code — roll handles the rest.
      </p>

      <div className="mt-10 flex w-full justify-center">
        <AskPanel />
      </div>

      <div className="mt-10 flex flex-wrap items-center justify-center gap-1.5 rounded-full border border-line bg-panel/70 p-1 backdrop-blur">
        {markStates.map((s) => (
          <button
            key={s}
            onClick={() => setState(s)}
            className={`rounded-full px-3 py-1 text-[12px] capitalize transition ${
              state === s ? "bg-fg text-bg" : "text-muted hover:text-fg"
            }`}
          >
            {s}
          </button>
        ))}
        <span className="mx-1 h-4 w-px bg-line-2" />
        {sequences.map((q) => (
          <button
            key={q.label}
            onClick={() => play(q.steps)}
            className="rounded-full px-3 py-1 text-[12px] text-accent transition hover:bg-accent/10"
          >
            ▶ {q.label}
          </button>
        ))}
      </div>
    </section>
  );
}
