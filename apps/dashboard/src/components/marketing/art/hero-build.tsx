import { Box, Database, GitPullRequest, HardDrive, Laptop, Rocket, Server, Zap, type LucideIcon } from "lucide-react";
import { Art, Beacon, Check, Chip, Cross, Dither, Glyph, T, Tile, flow, pulse } from "./kit";

/** The pictures on the build-and-run products' own pages (see hero-data.tsx for the rules). */
const W = 560;
const H = 400;

const BURST_1 = [16, 30, 46, 62, 78, 70, 84, 66, 76, 58, 68, 48, 56, 38, 44, 28, 20, 12];
const BURST_2 = [8, 18, 30, 46, 60, 74, 66, 82, 72, 90, 80, 94, 86, 98, 88, 96, 84, 92, 86];

/** Apps: a push becomes a deployment that's kept, and its instances follow the requests: to zero, and back. */
export function AppsHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 24)">
        <Chip x={16} y={12} w={76}>
          git push
        </Chip>
        <path d="M92,23 H124" className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4, 7)} />
        <Chip x={124} y={12} w={100}>
          build 11.8 s
        </Chip>
        <path d="M224,23 H256" className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4, 7)} />
        <Chip x={256} y={12} w={92} tone="fg" line="stroke-fg">
          dep_7c1e
        </Chip>
        <rect x={376} y={12} width={80} height={22} rx={11} strokeDasharray="3 3" className="stroke-line-2" />
        <T x={416} y={26.5} tone="faint" anchor="middle" size={9.5}>
          dep_7c1d
        </T>
        <rect x={464} y={12} width={80} height={22} rx={11} strokeDasharray="3 3" className="stroke-line-2" />
        <T x={504} y={26.5} tone="faint" anchor="middle" size={9.5}>
          dep_7c1c
        </T>
        <T x={544} y={52} tone="faint" anchor="end" size={8.5}>
          kept: a rollback runs one again
        </T>
        <T x={302} y={52} tone="muted" anchor="middle" size={8.5}>
          live
        </T>

        <path d="M16,76 H544" className="stroke-line" />
        <T x={16} y={102} tone="faint" size={8.5}>
          instances
        </T>
        <path d="M16,126 H150 V156 H200 V186" className="stroke-muted" />
        <path d="M200,186 H350" className="stroke-subtle" strokeDasharray="3 4" />
        <path d="M350,186 V156 H420 V126 H560" className="stroke-fg" />
        <T x={22} y={120} tone="muted">
          2
        </T>
        <T x={428} y={120} tone="muted">
          2
        </T>
        <T x={275} y={178} tone="subtle" anchor="middle">
          0 · asleep
        </T>
        <Beacon cx={350} cy={186} />
        <T x={360} y={204} tone="accent" size={8.5}>
          resumes in 13 ms
        </T>

        <path d="M16,330 H560" className="stroke-line-2" />
        {BURST_1.map((h, i) => (
          <Dither key={i} x={21 + i * 10} y={330 - h} width={7} height={h} level={50} />
        ))}
        {BURST_2.map((h, i) => (
          <Dither key={i} x={353 + i * 10} y={330 - h} width={7} height={h} level={75} />
        ))}
        <T x={16} y={350} tone="faint" size={8.5}>
          requests
        </T>
        <T x={275} y={322} tone="faint" anchor="middle" size={8.5}>
          idle: nothing billed
        </T>
      </g>
    </Art>
  );
}

const STACK: { icon: LucideIcon; dx: number }[] = [
  { icon: Rocket, dx: 34 },
  { icon: Database, dx: 82 },
  { icon: Zap, dx: 130 },
  { icon: HardDrive, dx: 178 },
];
const COPIES = [
  { name: "pr-1240", host: "web-pr1240-shop.preview.roll.site", y: 30, awake: true },
  { name: "pr-1241", host: "web-pr1241-shop.preview.roll.site", y: 206, awake: false },
];

/** Previews: the project on main, and a copy of all of it for each pull request, asleep until someone opens it. */
export function PreviewsHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 12)">
        <T x={16} y={86} tone="fg" size={10.5}>
          main
        </T>
        <T x={206} y={86} tone="faint" anchor="end" size={8.5}>
          project shop
        </T>
        <rect x={16} y={96} width={190} height={170} rx={12} className="stroke-subtle" />
        <path d="M111,152 C111,176 63,170 63,194 M111,152 V194 M111,152 C111,176 159,170 159,194" className="stroke-line-2" />
        <Tile cx={111} cy={134} s={36} icon={Rocket} line="stroke-subtle" ink="text-fg" />
        <Tile cx={63} cy={212} s={36} icon={Database} line="stroke-subtle" ink="text-fg" />
        <Tile cx={111} cy={212} s={36} icon={Zap} line="stroke-subtle" ink="text-fg" />
        <Tile cx={159} cy={212} s={36} icon={HardDrive} line="stroke-subtle" ink="text-fg" />

        <Glyph icon={GitPullRequest} cx={268} cy={181} size={15} className="text-muted" />
        {COPIES.map((c, i) => (
          <g key={c.name}>
            <path d={`M282,${i === 0 ? 172 : 190} C304,${i === 0 ? 150 : 212} 300,${c.y + 75} 330,${c.y + 75}`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.5 + i * 0.4, 7)} />
            <rect x={330} y={c.y} width={214} height={150} rx={12} strokeDasharray={c.awake ? undefined : "4 4"} className={c.awake ? "stroke-subtle" : "stroke-line-2"} />
            <T x={346} y={c.y + 26} tone={c.awake ? "fg" : "muted"} size={10.5}>
              {c.name}
            </T>
            <T x={528} y={c.y + 26} tone={c.awake ? "accent" : "faint"} anchor="end" size={8.5}>
              {c.awake ? "awake" : "asleep"}
            </T>
            {STACK.map((n) => (
              <Tile key={n.dx} cx={330 + n.dx + 2} cy={c.y + 74} s={32} icon={n.icon} line={c.awake ? "stroke-subtle" : "stroke-line-2"} ink={c.awake ? "text-fg" : "text-subtle"} dashed={!c.awake} />
            ))}
            <T x={346} y={c.y + 126} tone="faint" size={8}>
              {c.host}
            </T>
          </g>
        ))}
        <path d="M206,181 H254" className="stroke-subtle" />
        <Beacon cx={492} cy={53} r={2.4} />
        <T x={16} y={292} tone="faint" size={8.5}>
          every service, and a branch
        </T>
        <T x={16} y={306} tone="faint" size={8.5}>
          of everything it stores
        </T>
      </g>
    </Art>
  );
}

const CHANGES = [
  { id: "chg_9a41", y: 56 },
  { id: "chg_9a42", y: 112 },
  { id: "chg_9a43", y: 168 },
  { id: "chg_9a44", y: 224 },
];

/** Code: changes from many agents join one queue, a batch is tested on main's tip, a failure drops out, the rest land. */
export function CodeHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 40)">
        {CHANGES.map((c, i) => (
          <g key={c.id}>
            <path d={`M16,${c.y} C110,${c.y} 130,168 204,168`} className="stroke-subtle" strokeDasharray="4 4" {...flow(1.5 + i * 0.3, 8)} />
            <circle cx={16} cy={c.y} r={2.6} className="fill-muted" />
            <T x={26} y={c.y - 8} tone="faint" size={8.5}>
              {c.id}
            </T>
          </g>
        ))}
        <path d="M16,292 H130" className="stroke-line-2" strokeDasharray="3 4" />
        <circle cx={16} cy={292} r={2.6} className="fill-subtle" />
        <T x={26} y={284} tone="faint" size={8.5}>
          chg_9a40
        </T>
        <Cross cx={142} cy={292} />
        <T x={156} y={295} tone="faint" size={8.5}>
          failed its checks: out, and the next goes on
        </T>

        <path d="M204,168 H384" className="stroke-subtle" />
        <rect x={228} y={142} width={128} height={52} rx={26} className="fill-panel stroke-fg" />
        {[262, 292, 322].map((x, i) => (
          <circle key={x} cx={x} cy={168} r={4} className="fill-accent" {...pulse(1.8, i * 0.3)} />
        ))}
        <T x={292} y={130} tone="muted" anchor="middle" size={8.5}>
          one batch, tested on main's tip
        </T>
        <circle cx={396} cy={168} r={12} className="fill-panel stroke-fg" />
        <Check cx={396} cy={168} />
        <path d="M408,168 H560" className="stroke-fg" />
        {[434, 462, 490, 518].map((x) => (
          <circle key={x} cx={x} cy={168} r={2.8} className="fill-fg" />
        ))}
        <T x={544} y={152} tone="fg" anchor="end" size={10.5}>
          main
        </T>
        <path d="M490,174 V216" className="stroke-subtle" strokeDasharray="2 3" {...flow(1.6, 5)} />
        <Chip x={424} y={216} w={120} h={24}>
          GitHub mirror
        </Chip>
        <T x={544} y={258} tone="faint" anchor="end" size={8.5}>
          fast-forward, commit ids intact
        </T>
      </g>
    </Art>
  );
}

const MACHINES: { icon: LucideIcon; name: string; y: number }[] = [
  { icon: Laptop, name: "laptop", y: 78 },
  { icon: Server, name: "build box", y: 204 },
  { icon: Box, name: "sandbox", y: 318 },
];
const THREADS = [
  { y: 30, from: 78, agent: "Claude Code", state: "working", bar: 96 },
  { y: 86, from: 78, agent: "Codex", state: "waiting", bar: 70 },
  { y: 156, from: 204, agent: "Claude Code", state: "working", bar: 84 },
  { y: 212, from: 204, agent: "Codex", state: "done", bar: 104 },
  { y: 298, from: 318, agent: "ACP agent", state: "working", bar: 76 },
] as const;

/** Agents: your machines, the threads running on each, one that delegated, and the one waiting for a yes. */
export function AgentsHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 22)">
        {MACHINES.map((m) => (
          <g key={m.name}>
            <Tile cx={38} cy={m.y} s={44} icon={m.icon} line="stroke-subtle" ink="text-fg" />
            <T x={16} y={m.y + 38} tone="faint" size={8.5}>
              {m.name}
            </T>
          </g>
        ))}
        {THREADS.map((t, i) => {
          const waiting = t.state === "waiting";
          const cy = t.y + 20;
          return (
            <g key={t.y}>
              <path
                d={`M60,${t.from} C104,${t.from} 100,${cy} 144,${cy}`}
                className={waiting ? "stroke-accent/70" : "stroke-subtle"}
                strokeDasharray={t.state === "done" ? undefined : "3 4"}
                {...(t.state === "done" ? {} : flow(1.5 + i * 0.25, 7))}
              />
              <rect x={144} y={t.y} width={214} height={40} rx={10} className={waiting ? "fill-panel stroke-accent/70" : "fill-panel stroke-line-2"} />
              <T x={172} y={t.y + 17} tone={waiting ? "fg" : "muted"} size={9.5}>
                {t.agent}
              </T>
              <rect x={172} y={t.y + 25} width={t.bar} height={3} rx={1.5} className="fill-line-2" />
              <T x={346} y={t.y + 24} tone={waiting ? "accent" : "faint"} anchor="end" size={8.5}>
                {waiting ? "needs you" : t.state}
              </T>
              {waiting ? (
                <Beacon cx={158} cy={cy} r={2.8} />
              ) : t.state === "done" ? (
                <Check cx={158} cy={cy} s={2.8} className="stroke-muted" />
              ) : (
                <circle cx={158} cy={cy} r={2.8} className="fill-muted" {...pulse(1.8, i * 0.4)} />
              )}
            </g>
          );
        })}

        <path d="M358,106 H392" className="stroke-accent/70" />
        <rect x={392} y={70} width={152} height={72} rx={10} className="fill-panel stroke-subtle" />
        <T x={406} y={92} tone="fg" size={9.5}>
          Run the migration?
        </T>
        <Chip x={406} y={106} w={62} h={22} tone="fg" line="stroke-accent" size={8.5}>
          Approve
        </Chip>
        <Chip x={474} y={106} w={48} h={22} tone="muted" size={8.5}>
          Deny
        </Chip>

        <path d="M358,176 C378,176 376,186 396,186" className="stroke-subtle" strokeDasharray="3 4" {...flow(1.6, 7)} />
        <rect x={396} y={166} width={148} height={40} rx={10} strokeDasharray="4 3" className="fill-panel stroke-line-2" />
        <circle cx={410} cy={186} r={2.8} className="fill-muted" {...pulse(1.8, 0.6)} />
        <T x={424} y={183} tone="muted" size={9.5}>
          Codex
        </T>
        <T x={424} y={197} tone="faint" size={8.5}>
          delegated: write tests
        </T>
      </g>
    </Art>
  );
}
