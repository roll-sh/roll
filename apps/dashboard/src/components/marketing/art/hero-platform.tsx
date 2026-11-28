import { Bot, Box, Database, GitMerge, HardDrive, KeyRound, Laptop, Rocket, Route, Server, ShieldCheck, User, Waves, Workflow, Zap, type LucideIcon } from "lucide-react";
import { Art, Beacon, Chip, Cross, Glyph, T, Tile, flow, pulse } from "./kit";

/** The pictures on the platform products' own pages (see hero-data.tsx for the rules). */
const W = 560;
const H = 400;

const KEYS: { key: string; version: string; from?: LucideIcon }[] = [
  { key: "DATABASE_URL", version: "v15", from: Database },
  { key: "REDIS_URL", version: "v3", from: Zap },
  { key: "S3_BUCKET", version: "v1", from: HardDrive },
  { key: "STRIPE_KEY", version: "v14" },
];
const READERS = ["roll run -- npm start", "GitHub Actions", "Kubernetes"];
const ENVIRONMENTS = [
  { name: "development", x: 120, w: 96 },
  { name: "staging", x: 224, w: 70 },
  { name: "production", x: 302, w: 90 },
];

/** Secrets: keys linked to the resources their values come from, read by whatever runs your code; one just rotated. */
export function SecretsHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 44)">
        <T x={16} y={30} tone="faint" size={8.5}>
          resources
        </T>
        <T x={120} y={30} tone="faint" size={8.5}>
          keys
        </T>
        <T x={544} y={30} tone="faint" anchor="end" size={8.5}>
          read by
        </T>
        {KEYS.map((k, i) => {
          const y = 44 + i * 54;
          const rotated = i === 0;
          return (
            <g key={k.key}>
              {k.from ? (
                <>
                  <path d={`M56,${y + 15} H120`} className={rotated ? "stroke-accent" : "stroke-subtle"} strokeDasharray="3 4" {...flow(rotated ? 1 : 1.8 + i * 0.3, 7)} />
                  <Tile cx={38} cy={y + 15} s={36} icon={k.from} line={rotated ? "stroke-subtle" : "stroke-line-2"} ink={rotated ? "text-fg" : "text-muted"} />
                </>
              ) : (
                <T x={16} y={y + 19} tone="faint" size={8.5}>
                  typed in
                </T>
              )}
              <rect x={120} y={y} width={190} height={30} rx={8} className={rotated ? "fill-panel stroke-subtle" : "fill-panel stroke-line-2"} />
              <T x={134} y={y + 19} tone={rotated ? "fg" : "muted"} size={9.5}>
                {k.key}
              </T>
              <T x={298} y={y + 19} tone={rotated ? "accent" : "faint"} anchor="end" size={8.5}>
                {k.version}
              </T>
              <path d={`M310,${y + 15} H352`} className="stroke-line-2" />
            </g>
          );
        })}
        <T x={134} y={90} tone="accent" size={8.5}>
          password rotated: every reader gets v15
        </T>
        <path d="M352,59 V221" className="stroke-line-2" />
        {READERS.map((r, i) => {
          const y = 71 + i * 54;
          return (
            <g key={r}>
              <path d={`M352,${y + 15} H392`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.3, 7)} />
              <Chip x={392} y={y} w={152} h={30} tone={i === 0 ? "fg" : "muted"} line={i === 0 ? "stroke-subtle" : "stroke-line-2"} size={9}>
                {r}
              </Chip>
            </g>
          );
        })}
        <Beacon cx={120} cy={59} r={2.6} />

        <path d="M16,264 H544" className="stroke-line" />
        <T x={16} y={303} tone="faint" size={8.5}>
          per environment
        </T>
        {ENVIRONMENTS.map((e, i) => (
          <Chip key={e.name} x={e.x} y={288} w={e.w} h={24} tone={i === 2 ? "fg" : "muted"} line={i === 2 ? "stroke-subtle" : "stroke-line-2"} size={9}>
            {e.name}
          </Chip>
        ))}
        <T x={544} y={303} tone="faint" anchor="end" size={8.5}>
          nothing written to disk
        </T>
      </g>
    </Art>
  );
}

const CALLERS: { icon: LucideIcon; name: string; note: string; y: number }[] = [
  { icon: User, name: "a person", note: "signed in", y: 78 },
  { icon: Workflow, name: "CI", note: "rlt_… read-only", y: 174 },
  { icon: Bot, name: "an agent", note: "MCP · /v1/mcp", y: 270 },
];
const ROUTES: { icon: LucideIcon; rule: "allow" | "ask" | "none"; y: number }[] = [
  { icon: Database, rule: "allow", y: 54 },
  { icon: Waves, rule: "allow", y: 114 },
  { icon: Rocket, rule: "ask", y: 174 },
  { icon: GitMerge, rule: "allow", y: 234 },
  { icon: KeyRound, rule: "none", y: 294 },
];

/** Access: every caller goes through one evaluator, and each product answers allow, ask a person first, or nothing. */
export function AccessHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 36)">
        {CALLERS.map((c, i) => (
          <g key={c.name}>
            <Tile cx={36} cy={c.y} s={40} icon={c.icon} line="stroke-subtle" ink="text-fg" />
            <T x={64} y={c.y - 2} tone="muted" size={9.5}>
              {c.name}
            </T>
            <T x={64} y={c.y + 12} tone="faint" size={8.5}>
              {c.note}
            </T>
            <path d={`M152,${c.y + 5} C184,${c.y + 5} 180,${174 + (i - 1) * 30} 212,${174 + (i - 1) * 30}`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.3, 7)} />
          </g>
        ))}
        <rect x={212} y={34} width={36} height={280} rx={18} className="fill-panel stroke-fg" />
        <Glyph icon={ShieldCheck} cx={230} cy={174} size={17} className="text-fg" />
        <T x={230} y={24} tone="muted" anchor="middle" size={8.5}>
          one evaluator
        </T>
        {ROUTES.map((r, i) => {
          const ask = r.rule === "ask";
          const none = r.rule === "none";
          return (
            <g key={i}>
              <path
                d={none ? `M248,${r.y} H286` : `M248,${r.y} H330`}
                className={ask ? "stroke-accent" : none ? "stroke-line-2" : "stroke-subtle"}
                strokeDasharray={none ? "2 4" : "3 4"}
                {...(none ? {} : flow(1.5 + i * 0.2, 7))}
              />
              {none && <Cross cx={296} cy={r.y} s={2.6} />}
              {ask && <rect x={330} y={r.y - 19} width={38} height={38} rx={10} className="fill-accent/15" {...pulse(2.4)} />}
              <Tile cx={349} cy={r.y} s={38} icon={r.icon} line={ask ? "stroke-accent" : none ? "stroke-line-2" : "stroke-subtle"} ink={ask ? "text-accent" : none ? "text-subtle/60" : "text-fg"} dashed={none} />
              <T x={378} y={r.y + 3} tone={ask ? "accent" : none ? "faint" : "muted"} size={8.5}>
                {none ? "no rule" : r.rule}
              </T>
            </g>
          );
        })}
        <path d="M400,171 H424" className="stroke-accent" strokeDasharray="3 4" {...flow(1.2, 7)} />
        <rect x={424} y={136} width={120} height={76} rx={10} className="fill-panel stroke-subtle" />
        <T x={436} y={158} tone="fg" size={9.5}>
          Deploy shop?
        </T>
        <T x={436} y={172} tone="faint" size={8.5}>
          asked by an agent
        </T>
        <Chip x={436} y={182} w={62} h={20} tone="fg" line="stroke-accent" size={8.5}>
          Approve
        </Chip>
        <T x={544} y={338} tone="faint" anchor="end" size={8.5}>
          the ask and the answer both go in the audit log
        </T>
      </g>
    </Art>
  );
}

const CLIENTS: { icon: LucideIcon; agent: string; api: string; y: number }[] = [
  { icon: Laptop, agent: "Claude Code", api: "Messages", y: 80 },
  { icon: Server, agent: "Codex", api: "Responses", y: 170 },
  { icon: Box, agent: "ACP agent", api: "Chat Completions", y: 260 },
];
const UPSTREAMS = [
  { name: "Anthropic", note: "429 · cooling down", y: 80 },
  { name: "Bedrock", note: "takes over", y: 170 },
  { name: "OpenAI", note: "", y: 260 },
];

/** Gateway: each machine's agent speaks its own API to one gateway, which sends it to your upstreams and moves on when one fails. */
export function GatewayHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 34)">
        {CLIENTS.map((c, i) => (
          <g key={c.agent}>
            <Tile cx={36} cy={c.y} s={40} icon={c.icon} line="stroke-subtle" ink="text-fg" />
            <T x={64} y={c.y - 2} tone="muted" size={9.5}>
              {c.agent}
            </T>
            <T x={64} y={c.y + 12} tone="faint" size={8.5}>
              {c.api}
            </T>
            <path d={`M158,${c.y + 5} C200,${c.y + 5} 194,170 236,170`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.3, 7)} />
          </g>
        ))}
        <T x={16} y={318} tone="faint" size={8.5}>
          a key for each machine
        </T>
        <rect x={236} y={144} width={52} height={52} rx={14} className="fill-panel stroke-fg" />
        <Glyph icon={Route} cx={262} cy={170} size={19} className="text-fg" />
        <T x={262} y={216} tone="muted" anchor="middle" size={8.5}>
          gateway
        </T>

        <path d="M288,170 C340,170 344,80 396,80" className="stroke-line-2" strokeDasharray="2 4" />
        <Cross cx={342} cy={124} s={2.8} />
        <path d="M288,170 H396" className="stroke-accent" strokeDasharray="3 4" {...flow(1.1, 7)} />
        <path d="M288,170 C340,170 344,260 396,260" className="stroke-line-2" />
        {UPSTREAMS.map((u, i) => (
          <g key={u.name}>
            <rect x={396} y={u.y - 17} width={148} height={34} rx={9} strokeDasharray={i === 0 ? "3 3" : undefined} className={i === 1 ? "fill-panel stroke-subtle" : "fill-panel stroke-line-2"} />
            <Glyph icon={KeyRound} cx={414} cy={u.y} size={13} className={i === 1 ? "text-fg" : "text-subtle"} />
            <T x={428} y={u.y + 3.5} tone={i === 1 ? "fg" : i === 0 ? "faint" : "muted"} size={9.5}>
              {u.name}
            </T>
            {u.note && (
              <T x={544} y={u.y + 32} tone={i === 1 ? "accent" : "faint"} anchor="end" size={8.5}>
                {u.note}
              </T>
            )}
          </g>
        ))}
        <Beacon cx={396} cy={170} r={2.6} />
        <T x={544} y={318} tone="faint" anchor="end" size={8.5}>
          your own API keys
        </T>
      </g>
    </Art>
  );
}
