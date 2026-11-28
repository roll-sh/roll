import { useReducedMotion } from "@roll/brand";
import { Check, ChevronRight, GitCommitHorizontal, GitPullRequestArrow, X } from "lucide-react";
import { AnimatePresence, LayoutGroup, MotionConfig, motion } from "motion/react";
import { useEffect, useReducer, useRef } from "react";
import { useInView } from "./in-view";
import { NOTE } from "./scene";

/** The changes that come round: what an afternoon of agents and people pushing looks like. */
const CHANGES: [title: string, by: string, diff: string][] = [
  ["Fix the login redirect", "claude · laptop", "+18 −4"],
  ["Add pagination to /v1/orders", "codex · build box", "+142 −9"],
  ["Rate-limit the search endpoint", "claude · sandbox", "+61 −2"],
  ["Bump pgx to 5.7", "ana", "+3 −3"],
  ["Cache the pricing table", "codex · laptop", "+77 −12"],
  ["Retry webhook deliveries", "claude · laptop", "+54 −8"],
  ["Drop the legacy cart flag", "sam", "+2 −96"],
  ["Index orders by customer", "codex · build box", "+29 −0"],
  ["Stream the export as CSV", "claude · sandbox", "+88 −15"],
  ["Paginate the audit log", "codex · laptop", "+64 −7"],
  ["Sign webhook payloads", "claude · laptop", "+41 −3"],
  ["Move sessions to Redis", "codex · build box", "+120 −58"],
  ["Tighten the CSP header", "ana", "+6 −2"],
  ["Batch the invoice emails", "claude · sandbox", "+93 −21"],
  ["Trim the Docker image", "sam", "+9 −31"],
  ["Add a health check to worker", "codex · laptop", "+22 −0"],
  ["Search: boost exact titles", "claude · laptop", "+15 −4"],
  ["Rotate the signing key", "codex · build box", "+37 −12"],
];
/** The checks a batch runs, as a repo's roll.toml would name them. */
const CHECKS = ["build", "test", "lint"];
/** The sim's clock, in ms, and how much of a check one tick runs. */
const TICK = 520;
const PER_TICK = 0.36;
/** Changes in a batch, waiting at most, and kept on screen once landed. */
const BATCH = 3;
const WAITING = 5;
const KEPT = 6;

interface Change {
  n: number;
  id: string;
  title: string;
  by: string;
  diff: string;
  /** Its tests fail: it will drop out of its batch. */
  bad: boolean;
  /** Pushed by the visitor. */
  mine: boolean;
}

interface Queue {
  ticks: number;
  next: number;
  waiting: Change[];
  batch: Change[];
  /** The check that's running (CHECKS.length once all passed), and how far it is. */
  check: number;
  progress: number;
  /** Ticks left before the next batch is built. */
  rest: number;
  dropped: { change: Change; at: number } | null;
  main: Change[];
  landed: number;
  drops: number;
  said: string;
}

const change = (n: number, mine = false): Change => {
  const [title, by, diff] = mine ? ["Your change", "you", "+1 −1"] : CHANGES[n % CHANGES.length];
  return { n, id: `chg_${((Math.imul(n + 17, 2654435761) >>> 0) % 0xffff).toString(16).padStart(4, "0")}`, title, by, diff, bad: !mine && n % 5 === 3, mine };
};

/** The board as it opens: a batch halfway through its tests, three behind it, main moving. */
const START: Queue = {
  ticks: 0,
  next: 14,
  waiting: [change(10), change(11), change(12), change(9)],
  batch: [change(6), change(7), change(8)],
  check: 1,
  progress: 0.3,
  rest: 0,
  dropped: null,
  main: [change(5), change(4), change(2), change(1), change(0), change(15)],
  landed: 6,
  drops: 1,
  said: "testing 3 changes together",
};

type Action = { type: "tick" } | { type: "push" };

/** One step of the queue. Pure, so the board is the same for the same clicks. */
function advance(q: Queue, action: Action): Queue {
  if (action.type === "push") {
    return { ...q, next: q.next + 1, waiting: [...q.waiting, change(q.next, true)] };
  }
  const s: Queue = { ...q, ticks: q.ticks + 1 };
  if (s.dropped && s.ticks - s.dropped.at > 9) s.dropped = null;
  if (s.waiting.length < WAITING && s.ticks % 3 === 0) {
    s.waiting = [...s.waiting, change(s.next)];
    s.next++;
  }
  if (s.batch.length === 0) {
    if (s.rest > 0) return { ...s, rest: s.rest - 1 };
    if (s.waiting.length === 0) return s;
    const take = s.waiting.slice(0, BATCH);
    return { ...s, waiting: s.waiting.slice(take.length), batch: take, check: 0, progress: 0, said: `testing ${count(take.length)} together, on main's tip` };
  }
  const progress = s.progress + PER_TICK;
  const bad = s.batch.find((c) => c.bad);
  // A change whose tests fail leaves the batch; the rest are tested again without it.
  if (bad && s.check === 1 && progress >= 0.7) {
    const rest = s.batch.filter((c) => c !== bad);
    return {
      ...s,
      batch: rest,
      check: 0,
      progress: 0,
      rest: rest.length ? 0 : 2,
      dropped: { change: bad, at: s.ticks },
      drops: s.drops + 1,
      said: rest.length ? `${bad.id} failed test and dropped out. Testing the other ${rest.length === 1 ? "one" : "two"} again` : `${bad.id} failed test and dropped out`,
    };
  }
  if (progress < 1) return { ...s, progress };
  if (s.check + 1 < CHECKS.length) return { ...s, check: s.check + 1, progress: 0 };
  // Every check passed on the tree that will be main: it lands in one step.
  return {
    ...s,
    batch: [],
    check: 0,
    progress: 0,
    rest: 2,
    main: [...[...s.batch].reverse(), ...s.main].slice(0, KEPT),
    landed: s.landed + s.batch.length,
    said: `${count(s.batch.length)} landed on main`,
  };
}

const count = (n: number) => `${n} change${n === 1 ? "" : "s"}`;

/**
 * roll code's merge queue, working: changes arrive from agents and people, the queue takes the
 * next few as a batch and runs the repo's checks on them together, on top of main; a change whose
 * tests fail drops out and the rest are tested again without it; what passes lands, and main moves
 * on, never having held a tree that wasn't tested. Push a change of your own and follow it across.
 * A concept, not a picture of the dashboard's queue page (that's in the tour). The clock stops
 * offscreen; under reduced motion the board is still, and Push a change steps it.
 */
export function MergeQueue({ className }: { className?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const reduced = useReducedMotion();
  const inView = useInView(ref);
  const [q, dispatch] = useReducer(advance, START);

  useEffect(() => {
    if (reduced || !inView) return;
    const t = setInterval(() => dispatch({ type: "tick" }), TICK);
    return () => clearInterval(t);
  }, [reduced, inView]);

  const push = () => {
    dispatch({ type: "push" });
    // Nothing moves by itself under reduced motion: a push walks the queue on a few steps.
    if (reduced) for (let i = 0; i < 4; i++) dispatch({ type: "tick" });
  };

  const testing = q.batch.length > 0;
  return (
    <MotionConfig reducedMotion="user" transition={{ duration: 0.42, ease: [0.23, 1, 0.32, 1] }}>
      <LayoutGroup>
        <div ref={ref} className={`grid grid-cols-[minmax(0,1fr)] gap-px bg-line lg:grid-cols-[minmax(0,1fr)_minmax(0,1.12fr)_minmax(0,1fr)] ${className ?? ""}`}>
          <Column
            n="1"
            title="Pushed"
            note="by agents and people, all day"
            action={
              <button
                onClick={push}
                className="inline-flex items-center gap-1.5 rounded-lg border border-line-2 px-2.5 py-1.5 text-[12px] text-fg transition duration-150 hover:bg-panel-2 active:scale-[0.97]"
              >
                <GitPullRequestArrow className="size-3.5 text-accent" strokeWidth={1.5} />
                Push a change
              </button>
            }
          >
            <div className="space-y-2">
              <AnimatePresence initial={false} mode="popLayout">
                {q.waiting.map((c, i) => (
                  <Card key={c.id} c={c} state="waiting" note={`position ${i + 1}`} className={i > 2 ? "max-lg:hidden" : ""} />
                ))}
              </AnimatePresence>
            </div>
          </Column>

          <Column n="2" title="The merge queue" note="a batch, tested once, on top of main">
            <div className={`rounded-xl border p-2 transition-colors duration-300 ${testing ? "border-line-2 bg-panel/60" : "border-dashed border-line-2"}`}>
              <p className="flex items-center gap-2 px-1.5 pt-0.5 pb-2 font-mono text-[10.5px] text-subtle">
                <span className={`size-1.5 rounded-full ${testing ? "bg-accent" : "bg-line-2"}`} />
                {testing ? `batch of ${q.batch.length}` : "waiting for the next batch"}
                <span className="ml-auto">main + {q.batch.map((c) => c.id.slice(4)).join(" + ") || "…"}</span>
              </p>
              <div className="min-h-[11rem] space-y-2">
                <AnimatePresence initial={false} mode="popLayout">
                  {q.batch.map((c) => (
                    <Card key={c.id} c={c} state="testing" />
                  ))}
                </AnimatePresence>
              </div>
              <div className="mt-3 space-y-2 border-t border-line px-1.5 pt-3 pb-1">
                {CHECKS.map((name, i) => {
                  const done = testing && i < q.check;
                  const on = testing && i === q.check;
                  return (
                    <div key={name} className="flex items-center gap-3 font-mono text-[11px]">
                      <span className={`w-9 ${on || done ? "text-fg" : "text-subtle"}`}>{name}</span>
                      <span className="h-1 flex-1 overflow-hidden rounded-full bg-line">
                        <span
                          className={`block h-full rounded-full ${on ? "bg-accent" : "bg-fg/60"}`}
                          style={{ width: `${done ? 100 : on ? Math.min(100, q.progress * 100) : 0}%`, transition: done || on ? `width ${TICK}ms linear` : "none" }}
                        />
                      </span>
                      <span className={`w-14 text-right ${on ? "text-accent" : done ? "text-muted" : "text-subtle"}`}>{done ? "passed" : on ? "running" : "queued"}</span>
                    </div>
                  );
                })}
              </div>
            </div>
            <p aria-live="polite" className="mt-3 min-h-[2.4rem] px-1 font-mono text-[11px] leading-relaxed text-muted">
              {q.said}
            </p>
            <div className="min-h-[3.4rem]">
              <AnimatePresence initial={false}>{q.dropped && <Card key={q.dropped.change.id} c={q.dropped.change} state="dropped" note="main never saw it" />}</AnimatePresence>
            </div>
          </Column>

          <Column
            n="3"
            title="main"
            note="only tested trees get here"
            action={
              <span className="font-mono text-[11px] text-subtle tabular-nums">
                landed <span className="text-fg">{q.landed}</span> · dropped <span className="text-fg">{q.drops}</span>
              </span>
            }
          >
            <div className="relative space-y-2 pl-5">
              <span aria-hidden className="absolute top-2 bottom-0 left-[5px] w-px bg-line-2" />
              <AnimatePresence initial={false} mode="popLayout">
                {q.main.map((c, i) => (
                  <Card key={c.id} c={c} state="landed" head={i === 0} className={i > 2 ? "max-lg:hidden" : ""} />
                ))}
              </AnimatePresence>
            </div>
          </Column>
        </div>
      </LayoutGroup>
    </MotionConfig>
  );
}

function Column({ n, title, note, action, children }: { n: string; title: string; note: string; action?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="min-w-0 bg-bg p-4 sm:p-5 lg:min-h-[28rem]">
      <div className="mb-4 flex min-h-[2.25rem] items-start gap-3">
        <div className="min-w-0">
          <p className="flex items-center gap-2 text-[14px] font-medium tracking-[-0.01em] text-fg">
            <span className={`${NOTE} text-muted`}>{n}</span>
            {title}
            {n !== "3" && <ChevronRight className="size-3.5 text-subtle max-lg:hidden" strokeWidth={1.5} />}
          </p>
          <p className="mt-0.5 text-[12.5px] text-subtle">{note}</p>
        </div>
        <div className="ml-auto shrink-0">{action}</div>
      </div>
      {children}
    </div>
  );
}

/** A change, wherever it is on the board: the same card moves from column to column. */
function Card({ c, state, note, head = false, className }: { c: Change; state: "waiting" | "testing" | "landed" | "dropped"; note?: string; head?: boolean; className?: string }) {
  const tone =
    state === "dropped" ? "border-danger/40 bg-danger/5" : state === "testing" ? "border-line-2 bg-bg" : c.mine && state === "waiting" ? "border-accent/50 bg-panel" : "border-line bg-panel";
  return (
    <motion.div
      layout
      layoutId={c.id}
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: state === "landed" && !head ? 0.72 : 1, y: 0 }}
      exit={{ opacity: 0, transition: { duration: 0.2 } }}
      className={`relative flex items-center gap-2.5 rounded-lg border px-2.5 py-2 ${tone} ${className ?? ""}`}
    >
      {state === "landed" && <span aria-hidden className={`absolute top-1/2 -left-[19px] size-[9px] -translate-y-1/2 rounded-full border-2 border-bg ${head ? "bg-fg" : "bg-subtle"}`} />}
      <span className="grid size-5 shrink-0 place-items-center">
        {state === "landed" ? (
          <Check className="size-3.5 text-fg" strokeWidth={2} />
        ) : state === "dropped" ? (
          <X className="size-3.5 text-danger" strokeWidth={2} />
        ) : state === "testing" ? (
          <span className="size-2 rounded-full bg-accent motion-safe:animate-pulse" />
        ) : (
          <GitCommitHorizontal className={`size-3.5 ${c.mine ? "text-accent" : "text-subtle"}`} strokeWidth={1.5} />
        )}
      </span>
      <span className="min-w-0 flex-1">
        <span className={`block truncate text-[13px] ${state === "dropped" ? "text-muted line-through decoration-danger/50" : "text-fg"}`}>{c.title}</span>
        <span className="block truncate font-mono text-[10.5px] text-subtle">
          {c.id} · {c.by} <span className="max-sm:hidden">· {c.diff}</span>
        </span>
      </span>
      {(note || head) && <span className={`shrink-0 font-mono text-[10.5px] ${state === "dropped" ? "text-danger" : head ? "text-fg" : "text-subtle"}`}>{head ? "HEAD" : note}</span>}
    </motion.div>
  );
}
