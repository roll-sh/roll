import { useReducedMotion } from "@roll/brand";
import { Box, Check, GitBranch, Laptop, Milestone, Server, Terminal as TerminalIcon, type LucideIcon } from "lucide-react";
import { useEffect, useRef, useState, type PointerEvent } from "react";
import { useSeen } from "./in-view";

type Status = "working" | "waiting" | "done";

/** A line of a thread's conversation: what was asked, what the agent said or ran, a checkpoint, a request. */
type Entry = { kind: "asked" | "said" | "ran" | "checkpoint"; text: string; from?: string };

interface Thread {
  id: string;
  /** Where its card sits, in percent of the canvas: on a wide canvas, and on a phone's. */
  at: [number, number];
  phone: [number, number];
  title: string;
  agent: string;
  machine: string;
  status: Status;
  line: string;
  talk: Entry[];
}

/** The machines agents run on. The sandbox's size and price are roll agents' own (lib/marketing.ts). */
const MACHINES: { id: string; icon: LucideIcon; name: string; note: string }[] = [
  { id: "laptop", icon: Laptop, name: "laptop", note: "roll agent up · claude, codex" },
  { id: "build box", icon: Server, name: "build box", note: "roll agent up · codex" },
  { id: "sandbox", icon: Box, name: "sandbox", note: "hosted · 2 vCPU, 4 GB" },
];

const THREADS: Thread[] = [
  {
    id: "orders",
    at: [1.5, 30],
    phone: [2, 2],
    title: "Add pagination to /v1/orders",
    agent: "Codex",
    machine: "build box",
    status: "working",
    line: "Running the checks in roll.toml",
    talk: [
      { kind: "asked", text: "Page /v1/orders with a cursor, 50 a page." },
      { kind: "ran", text: "edit src/routes/orders.rs" },
      { kind: "said", text: "Delegated the migration and a review, so they run while I wire the route." },
      { kind: "checkpoint", text: "checkpoint 3" },
      { kind: "ran", text: "roll code check" },
    ],
  },
  {
    id: "review",
    at: [37, 4],
    phone: [51, 2],
    title: "Review: pagination",
    agent: "Claude Code",
    machine: "laptop",
    status: "waiting",
    line: "Asks to run cargo test -p orders",
    talk: [
      { kind: "asked", from: "Add pagination to /v1/orders", text: "Review the cursor handling before this lands." },
      { kind: "ran", text: "read src/routes/orders.rs" },
      { kind: "said", text: "A cursor past the last page isn't checked. I'd like to run the tests before I say so." },
    ],
  },
  {
    id: "migration",
    at: [37, 58],
    phone: [51, 35],
    title: "Write the migration",
    agent: "Codex",
    machine: "build box",
    status: "working",
    line: "Editing 0042_orders_cursor.sql",
    talk: [
      { kind: "asked", from: "Add pagination to /v1/orders", text: "Add an index for the cursor on orders(created_at, id)." },
      { kind: "ran", text: "roll db branch shop agent-migration" },
      { kind: "said", text: "Trying it on a branch of the database first." },
      { kind: "ran", text: "edit migrations/0042_orders_cursor.sql" },
    ],
  },
  {
    id: "login",
    at: [1.5, 70],
    phone: [2, 35],
    title: "Fix the login redirect",
    agent: "Claude Code",
    machine: "laptop",
    status: "done",
    line: "Landed chg_9a41 on main",
    talk: [
      { kind: "asked", text: "After login, go back to the page the visitor came from." },
      { kind: "ran", text: "edit src/auth/redirect.ts" },
      { kind: "checkpoint", text: "checkpoint 2" },
      { kind: "ran", text: "roll code land acme/web --base main" },
      { kind: "said", text: "Landed as chg_9a41: the checks passed in the merge queue." },
    ],
  },
  {
    id: "spike",
    at: [72.5, 6],
    phone: [2, 68],
    title: "Rate limiter: a spike",
    agent: "Claude Code",
    machine: "sandbox",
    status: "done",
    line: "Handed off with its notes",
    talk: [
      { kind: "asked", text: "Find out whether a token bucket fits the API's limits." },
      { kind: "ran", text: "cargo bench -p limiter" },
      { kind: "said", text: "It fits. Handing the real change to a thread on the laptop, with what I found." },
      { kind: "checkpoint", text: "checkpoint 5" },
    ],
  },
  {
    id: "limiter",
    at: [72.5, 62],
    phone: [51, 68],
    title: "Rate limiter: token bucket",
    agent: "Claude Code",
    machine: "laptop",
    status: "working",
    line: "Reading src/limiter.rs",
    talk: [
      { kind: "asked", from: "Rate limiter: a spike", text: "Build the token bucket the spike found, behind a flag." },
      { kind: "ran", text: "read src/limiter.rs" },
      { kind: "said", text: "Starting from the spike's checkpoint, so nothing is redone." },
    ],
  },
];

/** What joins two threads: work one gave another and waits on, or work passed on for good. */
const EDGES: { from: string; to: string; label: string }[] = [
  { from: "orders", to: "review", label: "delegated" },
  { from: "orders", to: "migration", label: "delegated" },
  { from: "spike", to: "limiter", label: "handed off" },
];

/** The waiting thread's request, and what answering it does. */
type Ask = "asking" | "running" | "passed" | "denied";
const ASKED: Record<Ask, { status: Status; line: string; talk: Entry[] }> = {
  asking: { status: "waiting", line: "Asks to run cargo test -p orders", talk: [] },
  running: { status: "working", line: "Running cargo test -p orders", talk: [{ kind: "ran", text: "cargo test -p orders" }] },
  passed: {
    status: "done",
    line: "Reviewed: one fix asked for",
    talk: [
      { kind: "ran", text: "cargo test -p orders" },
      { kind: "said", text: "14 passed, none for a cursor past the end. Sent that back as the one thing to fix." },
    ],
  },
  denied: { status: "done", line: "Stopped: you denied cargo test", talk: [{ kind: "said", text: "Stopped here: reviewed by reading only, and said so." }] },
};
const RUN_MS = 3200;
/** After an answer plays out, the request comes back for the next visitor. */
const RESET_MS = 9000;
/** Below this canvas width the cards take the phone's places. */
const PHONE = 560;

type Size = { w: number; h: number };
type Pos = Record<string, { x: number; y: number }>;
const places = (phone: boolean): Pos => Object.fromEntries(THREADS.map((t) => [t.id, { x: (phone ? t.phone : t.at)[0], y: (phone ? t.phone : t.at)[1] }]));

/**
 * roll agents as a working picture, not a picture of its page (that's a real render, in the tour):
 * threads as cards you can drag, the delegations and the handoff between them, and a panel beside
 * them with three tabs: the thread you picked, all of them, and the machines they run on. One
 * thread is waiting on a person:
 * answer it and it goes on. Cards come in once when the picture is first seen; reduced motion
 * shows it settled.
 */
export function AgentsCanvas({ className }: { className?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLDivElement>(null);
  const cardRefs = useRef<Record<string, HTMLDivElement | null>>({});
  const drawn = useSeen(ref);
  const reduced = useReducedMotion();
  const seen = drawn || reduced;
  const [phone, setPhone] = useState(false);
  const [pos, setPos] = useState<Pos>(() => places(false));
  // Each card's size as a percentage of the canvas, so edges meet its centre wherever it's dragged.
  const [sizes, setSizes] = useState<Record<string, Size>>({});
  const [ask, setAsk] = useState<Ask>("asking");
  const [picked, setPicked] = useState("review");
  const [tab, setTab] = useState<"thread" | "threads" | "machines">("thread");
  const [machine, setMachine] = useState<string | null>(null);
  const [dragging, setDragging] = useState<string | null>(null);
  const drag = useRef<{ id: string; dx: number; dy: number } | null>(null);
  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    let wasPhone: boolean | null = null;
    const measure = () => {
      const r = canvas.getBoundingClientRect();
      if (!r.width || !r.height) return;
      const isPhone = r.width < PHONE;
      const next: Record<string, Size> = {};
      for (const t of THREADS) {
        const el = cardRefs.current[t.id];
        if (el) next[t.id] = { w: (el.offsetWidth / r.width) * 100, h: (el.offsetHeight / r.height) * 100 };
      }
      setSizes(next);
      // Cards go back to their places when the canvas changes shape, and stay inside it otherwise.
      if (isPhone !== wasPhone) {
        wasPhone = isPhone;
        setPhone(isPhone);
        setPos(places(isPhone));
        return;
      }
      setPos((p) =>
        Object.fromEntries(
          Object.entries(p).map(([id, { x, y }]) => {
            const s = next[id];
            return [id, s ? { x: Math.max(0, Math.min(x, 100 - s.w)), y: Math.max(0, Math.min(y, 100 - s.h)) } : { x, y }];
          }),
        ),
      );
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(canvas);
    Object.values(cardRefs.current).forEach((el) => el && ro.observe(el));
    const pending = timers.current;
    return () => {
      ro.disconnect();
      pending.forEach(clearTimeout);
    };
  }, []);

  const sizeOf = (id: string) => sizes[id] ?? { w: 28, h: 24 };
  const centre = (id: string) => ({ x: pos[id].x + sizeOf(id).w / 2, y: pos[id].y + sizeOf(id).h / 2 });

  const answer = (allow: boolean) => {
    timers.current.forEach(clearTimeout);
    timers.current = [];
    const later = (ms: number, fn: () => void) => timers.current.push(setTimeout(fn, ms));
    setPicked("review");
    setTab("thread");
    if (allow) {
      setAsk("running");
      later(RUN_MS, () => setAsk("passed"));
      later(RUN_MS + RESET_MS, () => setAsk("asking"));
    } else {
      setAsk("denied");
      later(RESET_MS, () => setAsk("asking"));
    }
  };

  const onDown = (id: string) => (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest("button")) return;
    setPicked(id);
    setTab("thread");
    const r = canvasRef.current!.getBoundingClientRect();
    drag.current = {
      id,
      dx: ((e.clientX - r.left) / r.width) * 100 - pos[id].x,
      dy: ((e.clientY - r.top) / r.height) * 100 - pos[id].y,
    };
    e.currentTarget.setPointerCapture(e.pointerId);
    setDragging(id);
  };
  const onMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    const r = canvasRef.current!.getBoundingClientRect();
    const { w, h } = sizeOf(d.id);
    const x = Math.min(100 - w, Math.max(0, ((e.clientX - r.left) / r.width) * 100 - d.dx));
    const y = Math.min(100 - h, Math.max(0, ((e.clientY - r.top) / r.height) * 100 - d.dy));
    setPos((p) => ({ ...p, [d.id]: { x, y } }));
  };
  const onUp = () => {
    drag.current = null;
    setDragging(null);
  };

  const threads = THREADS.map((t) => (t.id === "review" ? { ...t, status: ASKED[ask].status, line: ASKED[ask].line, talk: [...t.talk, ...ASKED[ask].talk] } : t));
  const open = threads.find((t) => t.id === picked)!;

  return (
    <div ref={ref} className={`grid grid-cols-[minmax(0,1fr)] gap-px bg-line lg:grid-cols-[minmax(0,1fr)_20rem] ${className ?? ""}`}>
      <div className="min-w-0 bg-bg">
        <div ref={canvasRef} className="relative h-[35rem] overflow-hidden sm:h-[34rem]">
          <div className="absolute inset-0 bg-[radial-gradient(var(--line-2)_1px,transparent_1px)] [background-size:18px_18px]" />
          <svg className="absolute inset-0 h-full w-full" viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden>
            {EDGES.map((e) => {
              const a = centre(e.from);
              const b = centre(e.to);
              const lit = picked === e.from || picked === e.to;
              return (
                <path
                  key={e.from + e.to}
                  d={`M ${a.x} ${a.y} C ${(a.x + b.x) / 2} ${a.y}, ${(a.x + b.x) / 2} ${b.y}, ${b.x} ${b.y}`}
                  fill="none"
                  vectorEffect="non-scaling-stroke"
                  strokeWidth={1.25}
                  strokeDasharray="4 4"
                  className={`transition-colors duration-200 ${lit ? "stroke-muted" : "stroke-line-2"}`}
                />
              );
            })}
          </svg>
          {EDGES.map((e) => {
            const a = centre(e.from);
            const b = centre(e.to);
            return (
              <span
                key={e.from + e.to}
                className="pointer-events-none absolute -translate-x-1/2 -translate-y-1/2 rounded-full border border-line bg-bg px-1.5 py-px font-mono text-[9.5px] whitespace-nowrap text-subtle"
                style={{ left: `${(a.x + b.x) / 2}%`, top: `${(a.y + b.y) / 2}%`, opacity: seen ? 1 : 0, transition: "opacity 300ms ease-out 600ms" }}
              >
                {e.label}
              </span>
            );
          })}
          {threads.map((t, i) => {
            const off = machine !== null && t.machine !== machine;
            const on = picked === t.id;
            return (
              <div
                key={t.id}
                ref={(el) => {
                  cardRefs.current[t.id] = el;
                }}
                onPointerDown={onDown(t.id)}
                onPointerMove={onMove}
                onPointerUp={onUp}
                onPointerCancel={onUp}
                className={`absolute touch-none rounded-xl border bg-panel p-2.5 select-none sm:p-3 ${phone ? "w-[47%]" : "w-[26%]"} ${
                  on ? "border-fg/35" : t.status === "waiting" ? "border-accent/45" : "border-line-2"
                } ${dragging === t.id ? "z-10 cursor-grabbing" : "cursor-grab"}`}
                style={{
                  left: `${pos[t.id].x}%`,
                  top: `${pos[t.id].y}%`,
                  opacity: !seen ? 0 : off ? 0.35 : 1,
                  transform: !seen ? "translateY(8px) scale(0.98)" : dragging === t.id ? "scale(1.02)" : "none",
                  transition: dragging
                    ? "transform 150ms cubic-bezier(0.23, 1, 0.32, 1)"
                    : `opacity 300ms ease-out ${seen && drawn ? 0 : i * 90}ms, transform 300ms cubic-bezier(0.23, 1, 0.32, 1) ${i * 90}ms, border-color 200ms`,
                }}
              >
                <div className="flex items-center gap-2">
                  <Dot status={t.status} />
                  <span className="truncate text-[12.5px] font-medium text-fg sm:text-[13px]">{t.title}</span>
                </div>
                <p className="mt-1 truncate font-mono text-[10.5px] text-subtle">
                  {t.agent} · {t.machine}
                </p>
                <div className="mt-2.5 flex items-center gap-2 rounded-lg bg-panel-2 px-2 py-1.5 text-[11.5px] sm:text-[12px]">
                  {t.status === "done" ? <GitBranch className="size-3.5 shrink-0 text-muted" strokeWidth={1.5} /> : <TerminalIcon className="size-3.5 shrink-0 text-muted" strokeWidth={1.5} />}
                  <span aria-live={t.id === "review" ? "polite" : undefined} className={`truncate ${t.status === "working" ? "shimmer" : "text-muted"}`}>
                    {t.line}
                  </span>
                </div>
                {t.status === "waiting" && <Answer onAnswer={answer} />}
              </div>
            );
          })}
        </div>
      </div>

      <div className="flex min-w-0 flex-col bg-bg lg:min-h-[34rem]">
        <div role="tablist" className="flex gap-1 border-b border-line p-2">
          {(
            [
              { id: "thread", label: open.status === "waiting" ? "Waiting on you" : "Thread", count: null },
              { id: "threads", label: "Threads", count: threads.length },
              { id: "machines", label: "Machines", count: MACHINES.length },
            ] as const
          ).map((t) => (
            <button
              key={t.id}
              role="tab"
              aria-selected={tab === t.id}
              onClick={() => setTab(t.id)}
              className={`flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-[12.5px] whitespace-nowrap transition-colors duration-200 ${tab === t.id ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
            >
              {t.id === "thread" && <Dot status={open.status} />}
              {t.label}
              {t.count !== null && <span className="font-mono text-[10.5px] text-subtle tabular-nums">{t.count}</span>}
            </button>
          ))}
        </div>

        {tab === "thread" && (
          <>
            <div className="px-5 pt-4">
              <p className="truncate text-[14px] font-medium text-fg">{open.title}</p>
              <p className="mt-0.5 font-mono text-[10.5px] text-subtle">
                {open.agent} · {open.machine}
              </p>
            </div>
            <ol key={open.id} className="flex-1 space-y-3 p-5">
              {open.talk.map((e, i) => (
                <li key={`${i}${e.text}`} className="rise" style={{ animationDelay: `${Math.min(i, 5) * 50}ms` }}>
                  <Said entry={e} live={open.status === "working" && i === open.talk.length - 1} />
                </li>
              ))}
              {open.status === "waiting" && (
                <li className="rise rounded-lg border border-accent/40 p-3">
                  <p className="text-[12.5px] text-fg">Run this command?</p>
                  <p className="mt-1 font-mono text-[11.5px] text-muted">cargo test -p orders</p>
                  <Answer onAnswer={answer} />
                </li>
              )}
            </ol>
          </>
        )}

        {tab === "threads" && (
          <ul className="flex-1 space-y-1 p-2">
            {threads.map((t) => (
              <li key={t.id}>
                <button
                  onClick={() => {
                    setPicked(t.id);
                    setTab("thread");
                  }}
                  className="flex w-full items-start gap-2.5 rounded-lg px-3 py-2.5 text-left transition-colors duration-200 hover:bg-panel"
                >
                  <span className="mt-1">
                    <Dot status={t.status} />
                  </span>
                  <span className="min-w-0">
                    <span className="block truncate text-[13px] text-fg">{t.title}</span>
                    <span className="block truncate font-mono text-[10.5px] text-subtle">
                      {t.agent} · {t.machine}
                    </span>
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}

        {tab === "machines" && (
          <div className="flex flex-1 flex-col p-2">
            {MACHINES.map((m) => {
              const on = machine === m.id;
              return (
                <button
                  key={m.id}
                  onClick={() => setMachine(on ? null : m.id)}
                  aria-pressed={on}
                  className={`flex items-start gap-3 rounded-lg px-3 py-2.5 text-left transition-colors duration-200 ${on ? "bg-panel" : "hover:bg-panel/60"}`}
                >
                  <m.icon className={`mt-0.5 size-4 shrink-0 ${on ? "text-fg" : "text-muted"}`} strokeWidth={1.5} />
                  <span className="min-w-0">
                    <span className="flex items-center gap-2 text-[13px] text-fg">
                      {m.name}
                      <span className="font-mono text-[10.5px] text-subtle tabular-nums">{threads.filter((t) => t.machine === m.id).length}</span>
                    </span>
                    <span className="block font-mono text-[10.5px] leading-snug text-subtle">{m.note}</span>
                  </span>
                </button>
              );
            })}
            <p className="mt-auto px-3 pt-4 pb-2 text-[12.5px] leading-relaxed text-subtle">Provider logins never leave the machine.</p>
          </div>
        )}
      </div>
    </div>
  );
}

function Answer({ onAnswer }: { onAnswer: (allow: boolean) => void }) {
  return (
    <div className="mt-2.5 flex gap-1.5">
      <button onClick={() => onAnswer(true)} className="rounded-md bg-fg px-2.5 py-1 text-[11px] font-medium text-bg transition duration-150 hover:opacity-85 active:scale-[0.97]">
        Allow
      </button>
      <button
        onClick={() => onAnswer(false)}
        className="rounded-md border border-line-2 px-2.5 py-1 text-[11px] text-muted transition duration-150 hover:text-fg active:scale-[0.97]"
      >
        Deny
      </button>
    </div>
  );
}

/** One line of a conversation, drawn by what it is. */
function Said({ entry, live }: { entry: Entry; live: boolean }) {
  if (entry.kind === "asked") {
    return (
      <div className="ml-6 rounded-lg bg-panel-2 px-3 py-2">
        {entry.from && <p className="mb-1 font-mono text-[10px] text-subtle">from {entry.from}</p>}
        <p className="text-[12.5px] leading-relaxed text-fg">{entry.text}</p>
      </div>
    );
  }
  if (entry.kind === "ran") {
    return (
      <p className="flex items-center gap-2 font-mono text-[11.5px]">
        <TerminalIcon className="size-3.5 shrink-0 text-subtle" strokeWidth={1.5} />
        <span className={`truncate ${live ? "shimmer" : "text-muted"}`}>{entry.text}</span>
      </p>
    );
  }
  if (entry.kind === "checkpoint") {
    return (
      <p className="flex items-center gap-2 font-mono text-[10.5px] text-subtle">
        <Milestone className="size-3.5 shrink-0" strokeWidth={1.5} />
        {entry.text}
        <span className="h-px flex-1 bg-line" />
      </p>
    );
  }
  return <p className={`text-[12.5px] leading-relaxed text-muted ${live ? "caret" : ""}`}>{entry.text}</p>;
}

function Dot({ status }: { status: Status }) {
  if (status === "done") return <Check className="size-3.5 shrink-0 text-muted" strokeWidth={2} />;
  if (status === "waiting") {
    return (
      <span className="relative grid size-3.5 shrink-0 place-items-center">
        <span className="absolute size-2.5 animate-ping rounded-full bg-accent/40 [animation-duration:2.2s] motion-reduce:hidden" />
        <span className="size-2 rounded-full bg-accent" />
      </span>
    );
  }
  return (
    <span className="grid size-3.5 shrink-0 place-items-center">
      <span className="size-2 rounded-full bg-fg/80 motion-safe:animate-pulse" />
    </span>
  );
}
