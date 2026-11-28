import { FileText, Film, Image, Search, Undo2 } from "lucide-react";
import { Art, Beacon, Chip, Dither, Glyph, T, flow, pulse } from "./kit";

/**
 * The pictures on the data products' own pages: each card's drawing (art/data.tsx) with room to
 * say the rest. 560×400, the same kit and the same rules: token inks, halftone for shade, one
 * accent, labels a person would type or read.
 */
const W = 560;
const H = 400;

/** A tick every five minutes over the last two hours. */
const TICKS = Array.from({ length: 25 }, (_, i) => 84 + i * 19);
/** main's compute over those two hours: [from, to, height], 40 a CU. */
const MAIN_CU = [
  [84, 179, 20],
  [179, 293, 40],
  [293, 388, 20],
  [388, 445, 10],
  [445, 540, 20],
];

/** Postgres: three branches on one history, each with compute of its own that goes to zero and comes back on a connection. */
export function PostgresHero() {
  return (
    <Art w={W} h={H}>
      {TICKS.map((x, i) => (
        <path key={x} d={`M${x},${i % 6 === 0 ? 24 : 29} V34`} className="stroke-line-2" />
      ))}
      <T x={84} y={16} tone="faint" anchor="middle">
        −2h
      </T>
      <T x={312} y={16} tone="faint" anchor="middle">
        −1h
      </T>
      <T x={540} y={16} tone="faint" anchor="middle">
        now
      </T>

      <T x={16} y={104} tone="fg" size={10.5}>
        main
      </T>
      <T x={16} y={119} tone="faint" size={8.5}>
        0.5 CU
      </T>
      {MAIN_CU.map(([from, to, h]) => (
        <Dither key={from} x={from + 1} y={108 - h} width={to - from - 2} height={h} level={h > 20 ? 75 : 50} />
      ))}
      <T x={236} y={61} tone="faint" anchor="middle" size={8.5}>
        1 CU
      </T>
      <path d="M84,108 H560" className="stroke-muted" />
      {TICKS.filter((_, i) => i % 2 === 0).map((x) => (
        <circle key={x} cx={x} cy={108} r={2.4} className="fill-fg" />
      ))}

      <T x={16} y={192} tone="muted" size={10.5}>
        dev
      </T>
      <T x={16} y={207} tone="faint" size={8.5}>
        0.25 CU
      </T>
      <path d="M198,108 C222,108 222,196 246,196 H331" className="stroke-subtle" />
      <path d="M331,196 H464" className="stroke-subtle" strokeDasharray="2 4" />
      <path d="M464,196 H560" className="stroke-subtle" />
      <Dither x={247} y={186} width={83} height={10} level={50} />
      <Dither x={465} y={186} width={75} height={10} level={50} />
      {[265, 303, 502].map((x) => (
        <circle key={x} cx={x} cy={196} r={2.2} className="fill-muted" />
      ))}
      <T x={397} y={188} tone="faint" anchor="middle" size={8.5}>
        asleep · 0 CU
      </T>
      <Chip x={440} y={132} w={48} h={20} size={9}>
        psql
      </Chip>
      <path d="M464,152 V188" className="stroke-subtle" strokeDasharray="2 3" {...flow(1.4, 5)} />
      <Beacon cx={464} cy={196} />
      <T x={474} y={176} tone="accent" size={8.5}>
        awake in 0.66 s
      </T>

      <T x={16} y={280} tone="muted" size={10.5}>
        pr-1240
      </T>
      <T x={16} y={295} tone="faint" size={8.5}>
        0.25 CU
      </T>
      <path d="M426,108 C450,108 450,284 474,284 H560" className="stroke-subtle" />
      <Dither x={475} y={274} width={65} height={10} level={50} />
      <circle cx={512} cy={284} r={2.2} className="fill-muted" />
      <T x={438} y={288} tone="faint" anchor="end" size={8.5}>
        branched in 205 ms
      </T>

      <T x={16} y={356} tone="faint" size={8.5}>
        object
      </T>
      <T x={16} y={368} tone="faint" size={8.5}>
        storage
      </T>
      {TICKS.slice(0, 24).map((x, i) => (
        <Dither key={x} x={x + 2} y={350} width={15} height={14} rx={3} level={i < 9 ? 75 : i < 17 ? 50 : 25} />
      ))}
      <T x={540} y={386} tone="faint" anchor="end" size={8.5}>
        WAL and base images: every branch restores from here
      </T>
    </Art>
  );
}

const KEYS = [
  { key: "cart:42", value: 92, ttl: "" },
  { key: "session:9f2c", value: 64, ttl: "58s" },
  { key: "rate:api:17", value: 22, ttl: "1s" },
  { key: "user:8:prefs", value: 78, ttl: "" },
  { key: "feed:home", value: 48, ttl: "30s" },
];
/** Each command, its reply, and how far along the line its text ends. */
const COMMANDS = [
  { cmd: "> GET cart:42", reply: '"3 items"', end: 104 },
  { cmd: "> SET session:9f2c … EX 60", reply: "OK", end: 190 },
  { cmd: "> INCR rate:api:17", reply: "(integer) 18", end: 136 },
];

/** Redis: commands and the keys they touch, and every write on its way to object storage. */
export function RedisHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 14)">
      {COMMANDS.map((c, i) => {
        const y = 43 + i * 44;
        const hit = i === 0;
        return (
          <g key={c.cmd}>
            <T x={16} y={y} tone="fg" size={10.5}>
              {c.cmd}
            </T>
            <T x={16} y={y + 18} tone="muted" size={10.5}>
              {c.reply}
            </T>
            <path
              d={`M${c.end},${y - 4} H264`}
              className={hit ? "stroke-accent" : "stroke-line-2"}
              strokeDasharray="3 4"
              {...flow(hit ? 1.1 : 1.8 + i * 0.3, 7)}
            />
          </g>
        );
      })}
      <T x={16} y={175} tone="fg" size={10.5}>
        {">"}
      </T>
      <rect x={30} y={165} width={1.4} height={13} className="fill-fg" {...pulse(1.1)} />

      {KEYS.map((k, i) => {
        const y = 24 + i * 44;
        const hit = i === 0;
        return (
          <g key={k.key}>
            <rect x={272} y={y} width={272} height={30} rx={8} className={hit ? "fill-panel stroke-subtle" : "fill-panel stroke-line-2"} />
            <T x={286} y={y + 19} tone={hit ? "fg" : "subtle"}>
              {k.key}
            </T>
            <Dither x={388} y={y + 10} width={k.value} height={10} level={hit ? 75 : 50} opacity={hit ? 1 : 0.6} />
            {k.ttl && (
              <T x={532} y={y + 19} tone="faint" anchor="end">
                {k.ttl}
              </T>
            )}
          </g>
        );
      })}
      <Beacon cx={272} cy={39} />

      <path d="M16,278 H544" className="stroke-line" />
      <T x={16} y={318} tone="muted">
        writes
      </T>
      <path d="M58,314 H92" className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4, 7)} />
      {Array.from({ length: 8 }, (_, i) => (
        <g key={i}>
          <rect x={98 + i * 38} y={302} width={30} height={24} rx={5} className="stroke-line-2" />
          <Dither x={102 + i * 38} y={306} width={22} height={16} level={i > 5 ? 75 : i > 2 ? 50 : 25} />
        </g>
      ))}
      <path d="M404,314 H438" className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4, 7)} />
      <T x={544} y={318} tone="muted" anchor="end">
        object storage
      </T>
      <T x={98} y={348} tone="faint" size={8.5}>
        one segment a second
      </T>
      <T x={544} y={348} tone="faint" anchor="end" size={8.5}>
        asleep: only this is billed
      </T>
      </g>
    </Art>
  );
}

const PARTITIONS = [8, 3, 10, 5, 9, 6];
const FILLED = new Set(["0-1", "0-3", "0-6", "1-0", "2-0", "2-2", "2-5", "2-8", "3-1", "3-4", "4-2", "4-4", "4-7", "5-0", "5-3"]);
const WAYS_IN = [
  { name: "cqlsh", x: 100, w: 64 },
  { name: "CQL driver", x: 222, w: 92 },
  { name: "DynamoDB SDK", x: 372, w: 108 },
];

/** NoSQL: three ways into one wide-column table, and a branch that's one manifest over the same SSTables. */
export function NosqlHero() {
  return (
    <Art w={W} h={H}>
      {WAYS_IN.map((c, i) => (
        <g key={c.name}>
          <Chip x={c.x} y={10} w={c.w}>
            {c.name}
          </Chip>
          <path d={`M${c.x + c.w / 2},32 V58`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.3, 7)} />
        </g>
      ))}
      <T x={16} y={56} tone="faint" size={8.5}>
        partition
      </T>
      {PARTITIONS.map((n, r) => {
        const y = 64 + r * 30;
        const on = r === 2;
        return (
          <g key={r}>
            <rect x={16} y={y} width={72} height={22} rx={6} className={on ? "stroke-fg" : "stroke-subtle"} />
            <T x={28} y={y + 15} tone={on ? "fg" : "subtle"}>
              user:{r + 1}
            </T>
            {Array.from({ length: n }, (_, c) => {
              const x = 100 + c * 45;
              const accent = on && c === 6;
              return (
                <g key={c}>
                  {FILLED.has(`${r}-${c}`) && <Dither x={x + 3} y={y + 3} width={33} height={16} level={on ? 50 : 25} />}
                  {accent && <rect x={x} y={y} width={39} height={22} rx={6} className="fill-accent/20" {...pulse(2.4)} />}
                  <rect x={x} y={y} width={39} height={22} rx={6} className={accent ? "stroke-accent" : on ? "stroke-subtle" : "stroke-line-2"} />
                </g>
              );
            })}
          </g>
        );
      })}

      <path d="M16,262 H544" className="stroke-line" />
      <Chip x={16} y={286} w={72} tone="fg" line="stroke-subtle">
        main
      </Chip>
      <Chip x={16} y={338} w={72} tone="fg" line="stroke-subtle">
        pr-482
      </Chip>
      <path d="M88,297 C140,297 132,326 184,326" className="stroke-subtle" />
      <path d="M88,349 C140,349 132,326 184,326" className="stroke-accent" strokeDasharray="3 4" {...flow(1.2, 7, { reverse: true })} />
      {Array.from({ length: 9 }, (_, i) => (
        <g key={i}>
          <rect x={190 + i * 40} y={312} width={32} height={28} rx={6} className="stroke-line-2" />
          <Dither x={194 + i * 40} y={316} width={24} height={20} level={i < 3 ? 75 : i < 6 ? 50 : 25} />
        </g>
      ))}
      <T x={190} y={302} tone="faint" size={8.5}>
        SSTables on object storage
      </T>
      <T x={100} y={376} tone="accent" size={8.5}>
        a branch is one manifest write
      </T>
    </Art>
  );
}

/** Each result: the widths of its title's words (with the ones that matched), its second line, its score. */
const RESULTS = [
  { words: [36, 60, 42, 26], hit: [0, 1, 2], line: 230, score: 100 },
  { words: [48, 60, 30], hit: [1], line: 190, score: 70 },
  { words: [36, 28, 42, 40], hit: [0, 2], line: 210, score: 44 },
  { words: [56, 42], hit: [1], line: 150, score: 22 },
];
/** The semantic side: documents as points, [x, y], the first three the nearest to the query. */
const POINTS = [
  [398, 108],
  [420, 124],
  [388, 132],
  [470, 96],
  [492, 136],
  [352, 100],
  [506, 104],
  [448, 142],
  [346, 138],
];

/** Search: one query, matched by its words and by its meaning, the two lists fused into one ranking. */
export function SearchHero() {
  return (
    <Art w={W} h={H}>
      <rect x={120} y={10} width={320} height={30} rx={15} className="stroke-subtle" />
      <Glyph icon={Search} cx={139} cy={25} size={13} />
      <T x={154} y={29} tone="fg" size={10.5}>
        trail running shoes
      </T>
      <rect x={277} y={18} width={1.4} height={14} className="fill-fg" {...pulse(1.1)} />
      <path d="M220,40 C220,58 140,56 140,76" className="stroke-subtle" strokeDasharray="3 4" {...flow(1.5, 7)} />
      <path d="M340,40 C340,58 420,56 420,76" className="stroke-subtle" strokeDasharray="3 4" {...flow(1.8, 7)} />

      <rect x={40} y={78} width={200} height={76} rx={10} className="stroke-line-2" />
      <T x={40} y={70} tone="faint" size={8.5}>
        by its words
      </T>
      {[
        [30, 44, 34, 22],
        [40, 44, 26],
        [30, 22, 34],
      ].map((row, r) => {
        let x = 54;
        return row.map((w, k) => {
          const at = x;
          x += w + 6;
          const hit = (r === 0 && k < 3) || (r === 1 && k === 1) || (r === 2 && k !== 1);
          return <rect key={`${r}${k}`} x={at} y={94 + r * 20} width={w} height={6} rx={3} className={hit ? "fill-fg/80" : "fill-line-2"} />;
        });
      })}

      <rect x={320} y={78} width={200} height={76} rx={10} className="stroke-line-2" />
      <T x={520} y={70} tone="faint" anchor="end" size={8.5}>
        by its meaning · E5
      </T>
      {POINTS.slice(0, 3).map(([x, y]) => (
        <path key={`${x}`} d={`M412,114 L${x},${y}`} className="stroke-subtle" />
      ))}
      {POINTS.map(([x, y], i) => (
        <circle key={`${x}${y}`} cx={x} cy={y} r={i < 3 ? 3 : 2.4} className={i < 3 ? "fill-fg" : "fill-subtle"} />
      ))}
      <circle cx={412} cy={114} r={5} className="fill-panel stroke-fg" />
      <circle cx={412} cy={114} r={22} className="stroke-line-2" strokeDasharray="2 3" />

      <path d="M140,154 C140,176 256,168 256,180" className="stroke-subtle" />
      <path d="M420,154 C420,176 304,168 304,180" className="stroke-subtle" />
      <Chip x={252} y={170} w={56} tone="fg" line="stroke-fg" fill="fill-panel">
        rrf
      </Chip>
      <path d="M280,192 V208" className="stroke-subtle" strokeDasharray="2 3" {...flow(1.4, 5)} />

      {RESULTS.map((r, i) => {
        const y = 220 + i * 42;
        let x = 64;
        return (
          <g key={i}>
            {i > 0 && <path d={`M40,${y - 10} H520`} className="stroke-line" />}
            <T x={42} y={y + 12} tone="faint">
              {i + 1}
            </T>
            {r.words.map((w, k) => {
              const at = x;
              x += w + 7;
              return <rect key={k} x={at} y={y + 2} width={w} height={6} rx={3} className={r.hit.includes(k) ? "fill-fg/80" : "fill-line-2"} />;
            })}
            <rect x={64} y={y + 16} width={r.line} height={4} rx={2} className="fill-line-2" />
            <rect x={420} y={y + 8} width={100} height={5} rx={2.5} className="fill-line-2" />
            <rect x={420} y={y + 8} width={r.score} height={5} rx={2.5} className={i === 0 ? "fill-accent" : "fill-subtle"} />
          </g>
        );
      })}
    </Art>
  );
}

const FILES = [Image, FileText, Film];

/** Storage: a bucket, a branch of it that shares every object until one changes, and the trash that deletes go to first. */
export function StorageHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 22)">
      <Chip x={16} y={10} w={104} tone="fg" line="stroke-subtle">
        s3://assets
      </Chip>
      <Chip x={312} y={10} w={120} tone="muted">
        assets-pr-42
      </Chip>
      {[0, 1, 2].map((r) =>
        [0, 1, 2, 3].map((c) => {
          const y = 46 + r * 58;
          const changed = r === 1 && c === 1;
          const Icon = FILES[(r + c) % 3];
          return (
            <g key={`${r}${c}`}>
              <rect x={16 + c * 58} y={y} width={48} height={48} rx={10} className="stroke-subtle" />
              <Glyph icon={Icon} cx={40 + c * 58} cy={y + 24} size={17} className="text-muted" />
              <rect x={312 + c * 58} y={y} width={48} height={48} rx={10} strokeDasharray={changed ? undefined : "3 3"} className={changed ? "stroke-fg" : "stroke-line-2"} />
              <Glyph icon={Icon} cx={336 + c * 58} cy={y + 24} size={17} className={changed ? "text-fg" : "text-subtle/60"} />
            </g>
          );
        }),
      )}
      <path d="M250,128 H298 M292,122 l6,6 l-6,6" className="stroke-subtle" strokeLinecap="round" strokeLinejoin="round" />
      <T x={274} y={116} tone="faint" anchor="middle" size={8.5}>
        branch
      </T>
      <T x={312} y={234} tone="faint" size={8.5}>
        shared with assets · one object changed
      </T>

      <rect x={16} y={270} width={528} height={76} rx={12} strokeDasharray="4 4" className="stroke-line-2" />
      <T x={34} y={304} tone="muted">
        trash
      </T>
      <T x={34} y={320} tone="faint" size={8.5}>
        7 days
      </T>
      {[0, 1, 2].map((i) => {
        const x = 110 + i * 54;
        const undo = i === 1;
        return (
          <g key={i}>
            <rect x={x} y={286} width={44} height={44} rx={9} strokeDasharray="3 3" className={undo ? "stroke-subtle" : "stroke-line-2"} />
            <Glyph icon={undo ? Undo2 : FILES[i]} cx={x + 22} cy={308} size={16} className={undo ? "text-accent" : "text-subtle/60"} />
          </g>
        );
      })}
      <T x={282} y={304} tone="accent" size={8.5}>
        restore logo.svg
      </T>
      <T x={282} y={320} tone="faint" size={8.5}>
        deleted and overwritten objects wait here first
      </T>
      </g>
    </Art>
  );
}
