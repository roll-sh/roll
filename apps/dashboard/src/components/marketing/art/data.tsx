import { FileText, Film, Image, Search, Undo2 } from "lucide-react";
import { Art, Beacon, Dither, Glyph, T, flow, pulse } from "./kit";

/** Postgres: main's history, a branch that sleeps, and a branch for a pull request. */
export function PostgresArt() {
  return (
    <Art>
      {[28, 50, 72, 94, 116, 138, 160, 182, 204, 226, 248, 270, 292].map((x, i) => (
        <path key={x} d={`M${x},${i % 3 === 0 ? 20 : 24} V28`} className="stroke-line-2" />
      ))}
      <T x={28} y={14} tone="faint" anchor="middle">
        −2h
      </T>
      <T x={160} y={14} tone="faint" anchor="middle">
        −1h
      </T>
      <T x={292} y={14} tone="faint" anchor="middle">
        now
      </T>

      <path d="M-10,62 H330" className="stroke-muted" />
      {[28, 72, 116, 160, 204, 248, 292].map((x) => (
        <circle key={x} cx={x} cy={62} r={2.6} className="fill-fg" />
      ))}
      <T x={14} y={50} tone="muted">
        main
      </T>

      <path d="M116,62 C146,62 146,112 176,112 H330" className="stroke-subtle" />
      {[212, 266].map((x) => (
        <circle key={x} cx={x} cy={112} r={2.4} className="fill-muted" />
      ))}
      <T x={182} y={102}>
        dev
      </T>
      <T x={182} y={128} tone="faint">
        idle · 0 CU
      </T>

      <path d="M248,62 C272,62 272,162 296,162 H330" className="stroke-subtle" strokeDasharray="4 4" {...flow(1.8, 8)} />
      <Beacon cx={296} cy={162} />
      <T x={284} y={182} tone="muted" anchor="end">
        pr-1240
      </T>
    </Art>
  );
}

const KEYS = [
  { key: "cart:42", value: 86, ttl: "" },
  { key: "session:9f2c", value: 60, ttl: "58s" },
  { key: "rate:api:17", value: 28, ttl: "1s" },
  { key: "user:8:prefs", value: 72, ttl: "" },
  { key: "feed:home", value: 48, ttl: "30s" },
];

/** Redis: a command, and the key it hits among the others. */
export function RedisArt() {
  return (
    <Art>
      <rect x={16} y={12} width={124} height={24} rx={12} className="stroke-line-2" />
      <T x={30} y={28} tone="fg" size={10}>
        GET cart:42
      </T>
      <rect x={100} y={18} width={1.4} height={12} className="fill-fg" {...pulse(1.1)} />
      <path d="M40,36 V50" className="stroke-subtle" strokeDasharray="2 3" />
      {KEYS.map((k, i) => {
        const y = 50 + i * 32;
        const hit = i === 0;
        return (
          <g key={k.key}>
            <rect x={16} y={y} width={330} height={24} rx={6} className={hit ? "stroke-subtle" : "stroke-line-2"} />
            <T x={30} y={y + 16} tone={hit ? "fg" : "subtle"}>
              {k.key}
            </T>
            <Dither x={132} y={y + 8} width={k.value} height={8} level={hit ? 75 : 50} opacity={hit ? 1 : 0.55} />
            {k.ttl && (
              <T x={302} y={y + 16} tone="faint" anchor="end">
                {k.ttl}
              </T>
            )}
          </g>
        );
      })}
      <Beacon cx={296} cy={62} />
    </Art>
  );
}

const ROWS = [5, 2, 6, 3, 6, 4];
const FILLED = new Set(["0-1", "0-3", "2-0", "2-2", "2-5", "3-1", "4-2", "4-4", "5-0"]);

/** NoSQL: a wide-column table, a partition per row, each with the columns it happens to have. */
export function NosqlArt() {
  return (
    <Art>
      <T x={16} y={14} tone="faint">
        partition
      </T>
      <T x={90} y={14} tone="faint">
        columns →
      </T>
      {ROWS.map((n, r) => {
        const y = 24 + r * 30;
        const on = r === 2;
        return (
          <g key={r}>
            <rect x={16} y={y} width={64} height={20} rx={5} className={on ? "stroke-fg" : "stroke-subtle"} />
            <T x={26} y={y + 14} tone={on ? "fg" : "subtle"}>
              user:{r + 1}
            </T>
            {Array.from({ length: n }, (_, c) => {
              const x = 90 + c * 42;
              const accent = on && c === 3;
              return (
                <g key={c}>
                  {FILLED.has(`${r}-${c}`) && <Dither x={x + 3} y={y + 3} width={30} height={14} level={on ? 50 : 25} />}
                  {accent && <rect x={x} y={y} width={36} height={20} rx={5} className="fill-accent/20" {...pulse(2.4)} />}
                  <rect x={x} y={y} width={36} height={20} rx={5} className={accent ? "stroke-accent" : on ? "stroke-subtle" : "stroke-line-2"} />
                </g>
              );
            })}
          </g>
        );
      })}
    </Art>
  );
}

/** Each result: the widths of its title's words (with the ones that matched), its second line, its score. */
const RESULTS = [
  { words: [26, 44, 30, 18], hit: [0, 1, 2], line: 150, score: 54 },
  { words: [34, 44, 22], hit: [1], line: 120, score: 38 },
  { words: [26, 20, 30, 28], hit: [0, 2], line: 136, score: 24 },
  { words: [40, 30], hit: [1], line: 96, score: 12 },
];

/** Search: a query, and what it found, best first. */
export function SearchArt() {
  return (
    <Art>
      <rect x={16} y={12} width={288} height={28} rx={14} className="stroke-line-2" />
      <Glyph icon={Search} cx={34} cy={26} size={13} />
      <T x={48} y={30} tone="fg" size={10}>
        trail running shoes
      </T>
      <rect x={164} y={19} width={1.4} height={13} className="fill-fg" {...pulse(1.1)} />
      {RESULTS.map((r, i) => {
        const y = 56 + i * 36;
        let x = 40;
        return (
          <g key={i}>
            {i > 0 && <path d={`M16,${y - 8} H330`} className="stroke-line" />}
            <T x={20} y={y + 10} tone="faint">
              {i + 1}
            </T>
            {r.words.map((w, k) => {
              const at = x;
              x += w + 6;
              return <rect key={k} x={at} y={y + 2} width={w} height={5} rx={2.5} className={r.hit.includes(k) ? "fill-fg/80" : "fill-line-2"} />;
            })}
            <rect x={40} y={y + 14} width={r.line} height={4} rx={2} className="fill-line-2" />
            <rect x={250} y={y + 7} width={54} height={4} rx={2} className="fill-line-2" />
            <rect x={250} y={y + 7} width={r.score} height={4} rx={2} className={i === 0 ? "fill-accent" : "fill-subtle"} />
          </g>
        );
      })}
    </Art>
  );
}

const FILES = [Image, FileText, Film];

/** Storage: objects in a bucket, one of them deleted and still there to bring back. */
export function StorageArt() {
  return (
    <Art>
      <rect x={16} y={12} width={104} height={24} rx={12} className="stroke-line-2" />
      <T x={30} y={28} tone="muted" size={10}>
        s3://assets
      </T>
      {FILES.map((icon, r) =>
        [0, 1, 2, 3, 4, 5].map((c) => {
          const x = 16 + c * 62;
          const y = 50 + r * 62;
          const undo = r === 1 && c === 2;
          const lit = r === 0 && c === 1;
          return (
            <g key={`${r}${c}`}>
              {r === 0 && c === 3 && <Dither x={x + 4} y={y + 4} width={44} height={44} rx={7} level={25} />}
              <rect
                x={x}
                y={y}
                width={52}
                height={52}
                rx={10}
                strokeDasharray={undo ? "3 3" : undefined}
                className={undo || lit ? "stroke-subtle" : "stroke-line-2"}
              />
              <Glyph icon={undo ? Undo2 : icon} cx={x + 26} cy={y + 26} size={18} className={undo ? "text-accent" : lit ? "text-fg" : "text-subtle"} />
            </g>
          );
        }),
      )}
    </Art>
  );
}
