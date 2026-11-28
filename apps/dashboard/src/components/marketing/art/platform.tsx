import { Database, GitMerge, HardDrive, KeyRound, Rocket, Route, Search, Waves, Zap, type LucideIcon } from "lucide-react";
import { Art, Beacon, Cross, Glyph, T, flow, pulse } from "./kit";

const LINKS: { key: string; icon: LucideIcon }[] = [
  { key: "DATABASE_URL", icon: Database },
  { key: "REDIS_URL", icon: Zap },
  { key: "S3_BUCKET", icon: HardDrive },
  { key: "SEARCH_URL", icon: Search },
];

/** Secrets: keys linked to the resources their values come from, one of them just rotated. */
export function SecretsArt() {
  return (
    <Art>
      {LINKS.map((l, i) => {
        const y = 22 + i * 42;
        const rotated = i === 0;
        return (
          <g key={l.key}>
            <rect x={14} y={y} width={126} height={26} rx={7} className={rotated ? "stroke-subtle" : "stroke-line-2"} />
            <T x={24} y={y + 17} tone={rotated ? "fg" : "muted"} size={9}>
              {l.key}
            </T>
            <path
              d={`M140,${y + 13} H266`}
              className={rotated ? "stroke-accent" : "stroke-subtle"}
              strokeDasharray="3 4"
              {...flow(rotated ? 1 : 1.8 + i * 0.3, 7, { reverse: true })}
            />
            <circle cx={282} cy={y + 13} r={15} className={rotated ? "fill-panel stroke-subtle" : "fill-panel stroke-line-2"} />
            <Glyph icon={l.icon} cx={282} cy={y + 13} className={rotated ? "text-fg" : "text-muted"} />
          </g>
        );
      })}
      <T x={203} y={29} tone="accent" anchor="middle" size={8.5}>
        rotated
      </T>
    </Art>
  );
}

const SCOPES: { icon: LucideIcon; rule: "allow" | "ask" | "none" }[] = [
  { icon: Database, rule: "allow" },
  { icon: Waves, rule: "allow" },
  { icon: Rocket, rule: "ask" },
  { icon: GitMerge, rule: "allow" },
  { icon: KeyRound, rule: "none" },
];

/** Access: one token, and what its rules let it do on each product: allow, ask a person, or nothing. */
export function AccessArt() {
  return (
    <Art>
      <rect x={96} y={14} width={128} height={28} rx={14} className="stroke-subtle" />
      <Glyph icon={KeyRound} cx={114} cy={28} size={13} className="text-fg" />
      <T x={126} y={32} tone="fg" size={10}>
        rlt_••••••••
      </T>
      {SCOPES.map((s, i) => {
        const x = 40 + i * 60;
        const ask = s.rule === "ask";
        const none = s.rule === "none";
        return (
          <g key={i}>
            <path
              d={`M160,42 C160,82 ${x},72 ${x},108`}
              className={ask ? "stroke-accent/80" : none ? "stroke-line-2" : "stroke-subtle"}
              strokeDasharray={none ? "2 4" : "3 4"}
              {...(none ? {} : flow(1.6 + i * 0.2, 7))}
            />
            {ask && <rect x={x - 20} y={108} width={40} height={40} rx={10} className="fill-accent/15" {...pulse(2.4)} />}
            <rect
              x={x - 20}
              y={108}
              width={40}
              height={40}
              rx={10}
              strokeDasharray={none ? "3 3" : undefined}
              className={ask ? "stroke-accent" : none ? "stroke-line-2" : "stroke-subtle"}
            />
            <Glyph icon={s.icon} cx={x} cy={128} size={16} className={ask ? "text-accent" : none ? "text-subtle/60" : "text-fg"} />
            <T x={x} y={166} tone={ask ? "accent" : none ? "faint" : "muted"} anchor="middle" size={8.5}>
              {none ? "—" : s.rule}
            </T>
          </g>
        );
      })}
      <Cross cx={263} cy={86} s={2.5} />
    </Art>
  );
}

const APIS = ["Messages", "Chat", "Responses"];

/** Gateway: three APIs in, your own keys out, and the next key taking over when one fails. */
export function GatewayArt() {
  return (
    <Art>
      {APIS.map((name, i) => {
        const y = 30 + i * 58;
        return (
          <g key={name}>
            <rect x={12} y={y} width={84} height={24} rx={12} className="stroke-line-2" />
            <T x={54} y={y + 16} tone="muted" anchor="middle" size={9}>
              {name}
            </T>
            <path d={`M96,${y + 12} C124,${y + 12} 118,100 142,100`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.3, 7)} />
          </g>
        );
      })}
      <rect x={142} y={82} width={36} height={36} rx={10} className="fill-panel stroke-fg" />
      <Glyph icon={Route} cx={160} cy={100} size={16} className="text-fg" />

      <path d="M178,100 C212,100 214,42 250,42" className="stroke-line-2" strokeDasharray="2 4" />
      <Cross cx={214} cy={64} s={2.5} />
      <path d="M178,100 H250" className="stroke-accent" strokeDasharray="3 4" {...flow(1.1, 7)} />
      <path d="M178,100 C212,100 214,158 250,158" className="stroke-line-2" />
      {[42, 100, 158].map((y, i) => (
        <g key={y}>
          <rect x={250} y={y - 14} width={56} height={28} rx={8} strokeDasharray={i === 0 ? "3 3" : undefined} className={i === 1 ? "stroke-subtle" : "stroke-line-2"} />
          <Glyph icon={KeyRound} cx={266} cy={y} size={13} className={i === 1 ? "text-fg" : "text-subtle"} />
          <T x={278} y={y + 3} tone={i === 1 ? "fg" : "faint"} size={8.5}>
            key {i + 1}
          </T>
        </g>
      ))}
      <Beacon cx={250} cy={100} r={2.4} />
      <T x={214} y={92} tone="accent" anchor="middle" size={8.5}>
        failover
      </T>
    </Art>
  );
}
