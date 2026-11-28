import { useReducedMotion } from "@roll/brand";
import type { LucideIcon } from "lucide-react";
import { useCallback, useId, useRef, useState, type ReactNode } from "react";
import { useSeen } from "./in-view";

/**
 * A scene: the frame a demo sits in, cut into panes by its own hairlines (the grid's gap is the
 * line). The demos return bare panes for it.
 */
export function Scene({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={`grid grid-cols-[minmax(0,1fr)] gap-px overflow-hidden rounded-2xl border border-line bg-line ${className ?? ""}`}>{children}</div>;
}

/** The small mono label that says what a part of a scene is. */
export const NOTE = "font-mono text-[10.5px] tracking-wide text-subtle uppercase";

const DOTS = { 25: [[0, 0]], 50: [[0, 0], [3, 3]], 75: [[0, 0], [3, 3], [3, 0]] } as const;

/**
 * The bento's halftone (`Dither` in art/kit.tsx) for anything that isn't a 320×200 drawing: it
 * fills its box with dots in the current colour, a quarter, half or three quarters of them on.
 * The dots never scale, so a bar is as crisp at any length.
 */
export function Halftone({ level = 50, className }: { level?: keyof typeof DOTS; className?: string }) {
  const id = useId().replace(/[^a-zA-Z0-9]/g, "");
  return (
    <svg aria-hidden className={`block size-full ${className ?? ""}`}>
      <defs>
        <pattern id={id} width={6} height={6} patternUnits="userSpaceOnUse">
          {DOTS[level].map(([x, y]) => (
            <rect key={`${x}${y}`} x={x} y={y} width={1.7} height={1.7} fill="currentColor" />
          ))}
        </pattern>
      </defs>
      <rect width="100%" height="100%" fill={`url(#${id})`} />
    </svg>
  );
}

export type VersusRow = {
  name: string;
  /** The measured value as its doc writes it. */
  value: string;
  /** Its length against the longest bar, 0 to 1. */
  share: number;
  /** roll's own bar: bright and dense. The thing it's measured against is faint. */
  own?: boolean;
};

/** A measurement beside what it was measured against: bars that grow in once they're seen. */
export function Versus({ rows, big = false, className }: { rows: VersusRow[]; big?: boolean; className?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const seen = useSeen(ref);
  const reduced = useReducedMotion();
  const shown = seen || reduced;
  return (
    <div ref={ref} className={`${big ? "space-y-5" : "space-y-3"} ${className ?? ""}`}>
      {rows.map((r, i) => (
        <div key={r.name}>
          <div className={`flex items-baseline justify-between gap-3 font-mono ${big ? "text-[12.5px]" : "text-[11px]"}`}>
            <span className={r.own ? "text-fg" : "text-subtle"}>{r.name}</span>
            <span className={`tabular-nums ${r.own ? "text-fg" : "text-muted"}`}>{r.value}</span>
          </div>
          <div className={`overflow-hidden border border-line ${big ? "mt-2 h-10 rounded-md" : "mt-1.5 h-4 rounded-[3px]"}`}>
            <div
              className={`h-full min-w-[3px] ${r.own ? "bg-fg/15 text-fg" : "text-muted"}`}
              style={{
                width: shown ? `${Math.max(0.4, r.share * 100)}%` : "0%",
                transition: reduced ? "none" : `width 1100ms cubic-bezier(0.23, 1, 0.32, 1) ${150 + i * 120}ms`,
              }}
            >
              <Halftone level={r.own ? 75 : 25} />
            </div>
          </div>
        </div>
      ))}
    </div>
  );
}

/**
 * Which of a scene's points is showing. They go round by themselves while the scene is on screen
 * and nobody's reading (hovering or focusing the points holds them), like the tour's stops.
 */
export function useSteps(count: number) {
  const [at, setAt] = useState(0);
  const [held, setHeld] = useState(false);
  const next = useCallback(() => setAt((a) => (a + 1) % count), [count]);
  return { at, setAt, next, held, setHeld };
}

export type Step = { title: string; body: string; icon?: LucideIcon };

/**
 * A scene's points as steps, Linear-style: each is a line and what it means, the one showing is
 * bright and its hairline fills while it has the floor. Picking one moves the picture beside it.
 * `row` lays them side by side from `lg`; otherwise they stack.
 */
export function Steps({
  steps,
  state,
  running,
  seconds = 7,
  row = false,
  className,
}: {
  steps: Step[];
  state: ReturnType<typeof useSteps>;
  /** Whether the scene is on screen: the steps only advance then. */
  running: boolean;
  seconds?: number;
  row?: boolean;
  className?: string;
}) {
  const reduced = useReducedMotion();
  const { at, setAt, next, held, setHeld } = state;
  return (
    <div
      onPointerEnter={(e) => e.pointerType === "mouse" && setHeld(true)}
      onPointerLeave={() => setHeld(false)}
      onFocus={() => setHeld(true)}
      onBlur={() => setHeld(false)}
      className={`grid grid-cols-[minmax(0,1fr)] gap-px bg-line ${row ? "sm:grid-cols-2 lg:grid-flow-col lg:auto-cols-fr lg:grid-cols-none" : ""} ${className ?? ""}`}
    >
      {steps.map((s, i) => {
        const on = i === at;
        return (
          <button
            key={s.title}
            type="button"
            aria-pressed={on}
            onClick={() => setAt(i)}
            className={`group/step relative flex flex-col bg-bg p-5 text-left transition-colors duration-200 sm:px-6 ${on ? "bg-panel" : "hover:bg-panel/60"}`}
          >
            <span className="absolute inset-x-0 top-0 h-px overflow-hidden">
              {on &&
                (reduced ? (
                  <span className="absolute inset-0 bg-fg/60" />
                ) : (
                  <span
                    key={at}
                    onAnimationEnd={next}
                    className="absolute inset-0 origin-left bg-fg/60"
                    style={{ animation: `tour ${seconds}s linear forwards`, animationPlayState: running && !held ? "running" : "paused" }}
                  />
                ))}
            </span>
            <span className="flex items-center gap-2.5">
              <span className={`font-mono text-[10.5px] tabular-nums transition-colors ${on ? "text-accent" : "text-subtle"}`}>{String(i + 1).padStart(2, "0")}</span>
              <span className={`text-[14.5px] font-medium tracking-[-0.01em] transition-colors ${on ? "text-fg" : "text-muted group-hover/step:text-fg"}`}>{s.title}</span>
              {s.icon && <s.icon className={`ml-auto size-4 transition-colors ${on ? "text-fg" : "text-subtle"}`} strokeWidth={1.5} />}
            </span>
            <span className={`mt-2 text-[13px] leading-relaxed text-pretty transition-colors ${on ? "text-muted" : "text-subtle"}`}>{s.body}</span>
          </button>
        );
      })}
    </div>
  );
}
