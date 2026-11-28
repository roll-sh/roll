import { Database, GitPullRequest, Laptop, Rocket, Server, Zap, type LucideIcon } from "lucide-react";
import { Art, Beacon, Check, Cross, Dither, Glyph, T, flow, pulse } from "./kit";

const BURST_1 = [22, 40, 58, 70, 52, 64, 44, 30, 16];
const BURST_2 = [8, 20, 38, 56, 72, 60, 78, 66, 84, 74, 88, 80, 86, 76];

/** Apps: requests over time, and the instances that serve them: two, none while nobody asks, then back. */
export function AppsArt() {
  return (
    <Art>
      <T x={16} y={20} tone="faint">
        instances
      </T>
      <path d="M16,44 H98 V80" className="stroke-muted" />
      <path d="M98,80 H204" className="stroke-subtle" strokeDasharray="3 4" />
      <path d="M204,80 V62 H262 V44 H330" className="stroke-fg" />
      <T x={22} y={38} tone="muted">
        2
      </T>
      <T x={151} y={72} anchor="middle">
        0 · asleep
      </T>
      <T x={270} y={38} tone="muted">
        2
      </T>
      <Beacon cx={204} cy={80} />

      <path d="M16,176 H330" className="stroke-line-2" />
      {BURST_1.map((h, i) => (
        <Dither key={i} x={21 + i * 9} y={176 - h} width={6} height={h} level={50} />
      ))}
      {BURST_2.map((h, i) => (
        <Dither key={i} x={207 + i * 9} y={176 - h} width={6} height={h} level={75} />
      ))}
      <T x={16} y={192} tone="faint">
        requests
      </T>
    </Art>
  );
}

const PROJECT: { icon: LucideIcon; x: number; y: number }[] = [
  { icon: Rocket, x: 64, y: 36 },
  { icon: Database, x: 34, y: 88 },
  { icon: Zap, x: 94, y: 88 },
];

/** A project in miniature: a service and the database and cache it uses. */
function Project({ x, y, copy = false }: { x: number; y: number; copy?: boolean }) {
  return (
    <g transform={`translate(${x} ${y})`}>
      <rect width={128} height={124} rx={10} strokeDasharray={copy ? "4 3" : undefined} className={copy ? "stroke-subtle" : "stroke-line-2"} />
      <path d="M64,50 C64,66 34,60 34,74 M64,50 C64,66 94,60 94,74" className={copy ? "stroke-subtle" : "stroke-line-2"} />
      {PROJECT.map((n) => (
        <g key={n.x}>
          <rect x={n.x - 14} y={n.y - 14} width={28} height={28} rx={7} className={copy ? "stroke-muted" : "stroke-subtle"} />
          <Glyph icon={n.icon} cx={n.x} cy={n.y} className={copy ? "text-fg" : "text-muted"} />
        </g>
      ))}
    </g>
  );
}

/** Previews: the project on main, and a copy of all of it for a pull request. */
export function PreviewsArt() {
  return (
    <Art>
      <T x={16} y={26} tone="muted">
        main
      </T>
      <Project x={14} y={36} />
      <path d="M148,98 H170 M165,93 l5,5 l-5,5" className="stroke-subtle" strokeLinecap="round" strokeLinejoin="round" />
      <Glyph icon={GitPullRequest} cx={159} cy={80} size={13} className="text-accent" />
      <T x={180} y={26} tone="fg">
        pr-1240
      </T>
      <Project x={178} y={36} copy />
      <T x={242} y={178} tone="faint" anchor="middle" size={8.5}>
        asleep until opened
      </T>
    </Art>
  );
}

/** Code: changes joining the queue, a batch being tested, one dropping out, the rest landing on main. */
export function CodeArt() {
  return (
    <Art>
      <T x={16} y={18} tone="faint">
        changes
      </T>
      <T x={182} y={74} tone="accent" anchor="middle" size={8.5}>
        testing
      </T>
      <T x={304} y={82} tone="muted" anchor="end">
        main
      </T>
      {[34, 66, 128].map((y, i) => (
        <path key={y} d={`M-10,${y} C60,${y} 76,96 128,96`} className="stroke-subtle" strokeDasharray="4 4" {...flow(1.6 + i * 0.3, 8)} />
      ))}
      <path d="M-10,162 C40,162 60,162 92,162" className="stroke-line-2" strokeDasharray="3 4" />
      <Cross cx={102} cy={162} />
      <T x={114} y={165} tone="faint" size={8.5}>
        failed its checks
      </T>

      <path d="M128,96 H218" className="stroke-subtle" />
      <circle cx={142} cy={96} r={3} className="fill-panel stroke-subtle" />
      <rect x={160} y={85} width={44} height={22} rx={11} className="fill-panel stroke-accent" />
      <circle cx={174} cy={96} r={3} className="fill-accent" {...pulse(1.8)} />
      <circle cx={190} cy={96} r={3} className="fill-accent" {...pulse(1.8, 0.3)} />
      <circle cx={227} cy={96} r={9} className="stroke-fg" />
      <Check cx={227} cy={96} />
      <path d="M236,96 H330" className="stroke-fg" />
      {[258, 284, 310].map((x) => (
        <circle key={x} cx={x} cy={96} r={2.6} className="fill-fg" />
      ))}
    </Art>
  );
}

const THREADS = [
  { y: 16, from: 52, agent: "Claude Code", state: "working" },
  { y: 60, from: 52, agent: "Codex", state: "waiting" },
  { y: 110, from: 144, agent: "Claude Code", state: "done" },
  { y: 154, from: 144, agent: "Codex", state: "working" },
] as const;

/** Agents: two machines, the threads running on each, and the one that's waiting for you. */
export function AgentsArt() {
  return (
    <Art>
      {[
        { icon: Laptop, y: 32, name: "laptop" },
        { icon: Server, y: 124, name: "build box" },
      ].map((m) => (
        <g key={m.name}>
          <rect x={16} y={m.y} width={40} height={40} rx={10} className="stroke-subtle" />
          <Glyph icon={m.icon} cx={36} cy={m.y + 20} size={16} className="text-fg" />
          <T x={16} y={m.y + 54} tone="faint" size={8.5}>
            {m.name}
          </T>
        </g>
      ))}
      {THREADS.map((t, i) => {
        const waiting = t.state === "waiting";
        return (
          <g key={t.y}>
            <path
              d={`M56,${t.from} C100,${t.from} 96,${t.y + 16} 140,${t.y + 16}`}
              className={waiting ? "stroke-accent/70" : "stroke-subtle"}
              strokeDasharray={t.state === "done" ? undefined : "3 4"}
              {...(t.state === "done" ? {} : flow(1.5 + i * 0.25, 7))}
            />
            <rect x={140} y={t.y} width={190} height={32} rx={8} className={waiting ? "fill-panel stroke-accent/70" : "fill-panel stroke-line-2"} />
            <T x={166} y={t.y + 14} tone={waiting ? "fg" : "muted"} size={9}>
              {t.agent}
            </T>
            <rect x={166} y={t.y + 20} width={waiting ? 64 : 88 - i * 10} height={3} rx={1.5} className="fill-line-2" />
            <T x={304} y={t.y + 19} tone={waiting ? "accent" : "faint"} anchor="end" size={8.5}>
              {waiting ? "needs you" : t.state}
            </T>
            {waiting ? (
              <Beacon cx={153} cy={t.y + 16} r={2.6} />
            ) : t.state === "done" ? (
              <Check cx={153} cy={t.y + 16} s={2.6} className="stroke-muted" />
            ) : (
              <circle cx={153} cy={t.y + 16} r={2.6} className="fill-muted" {...pulse(1.8, i * 0.4)} />
            )}
          </g>
        );
      })}
    </Art>
  );
}
