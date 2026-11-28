import { Art, Beacon, Check, Cross, Dither, T, flow, pulse } from "./kit";

const SPECS = ["checkout.spec.ts", "cart.spec.ts", "login.spec.ts"];

/** Your own browser tests on a preview: the specs running against it, and a snapshot going to its visual review. */
export function PreviewTestsArt() {
  return (
    <Art>
      <rect x={14} y={14} width={186} height={140} rx={9} className="stroke-line-2" />
      <path d="M14,38 H200" className="stroke-line-2" />
      {[26, 36, 46].map((x) => (
        <circle key={x} cx={x} cy={26} r={2.2} className="fill-subtle" />
      ))}
      <T x={60} y={29} tone="faint" size={9.5}>
        pr-1240.shop.roll.app
      </T>
      {SPECS.map((spec, i) => {
        const y = 62 + i * 30;
        const running = i === 2;
        return (
          <g key={spec}>
            {running ? <circle cx={32} cy={y} r={3} className="fill-accent" {...pulse(1.6)} /> : <Check cx={32} cy={y} s={3} className="stroke-fg" />}
            <T x={46} y={y + 3} tone={running ? "fg" : "muted"} size={10}>
              {spec}
            </T>
            <path d={`M46,${y + 11} H${running ? 120 : 184}`} className={running ? "stroke-accent" : "stroke-line-2"} />
          </g>
        );
      })}
      <path d="M200,84 H228" className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4, 7)} />

      <rect x={228} y={40} width={64} height={48} rx={5} className="fill-panel stroke-subtle" />
      <Dither x={234} y={46} width={52} height={12} level={25} />
      <Dither x={234} y={64} width={30} height={18} level={50} />
      <rect x={244} y={70} width={64} height={48} rx={5} className="fill-panel stroke-muted" />
      <Dither x={250} y={76} width={52} height={12} level={25} />
      <Dither x={250} y={94} width={30} height={18} level={50} />
      <Dither x={284} y={94} width={18} height={18} level={75} />
      <rect x={282} y={92} width={22} height={22} rx={3} className="stroke-accent" />
      <T x={228} y={138} tone="faint" size={9.5}>
        snapshot
      </T>
      <T x={228} y={150} tone="muted" size={9.5}>
        to visual review
      </T>
      <T x={14} y={178} tone="faint" size={9.5}>
        playwright test · on the preview
      </T>
    </Art>
  );
}

const BUILDS = [
  { name: "roll", y: 70 },
  { name: "rolld", y: 122 },
];

/** rolld beside the CLI: the same installer, two binaries, each build checked against its checksum. */
export function RolldArt() {
  return (
    <Art>
      <rect x={14} y={14} width={292} height={30} rx={8} className="stroke-line-2" />
      <T x={26} y={33} tone="faint">
        $
      </T>
      <T x={40} y={33} tone="fg">
        install.sh --with-rolld
      </T>
      <rect x={196} y={23} width={1.4} height={12} className="fill-fg" {...pulse(1.1)} />
      <path d="M40,44 V70 M40,70 H58 M40,70 V122 H58" className="stroke-subtle" strokeDasharray="2 3" {...flow(1.6, 5)} />
      {BUILDS.map((b, k) => (
        <g key={b.name}>
          <rect x={58} y={b.y - 18} width={248} height={36} rx={8} className={k === 1 ? "stroke-muted" : "stroke-line-2"} />
          <T x={70} y={b.y + 4} tone={k === 1 ? "fg" : "muted"}>
            {b.name}
          </T>
          {["linux", "macOS"].map((os, i) => (
            <g key={os}>
              <T x={128 + i * 82} y={b.y + 4} tone="faint" size={9.5}>
                {os}
              </T>
              <Check cx={170 + i * 82} cy={b.y + 1} s={2.8} className={k === 1 ? "stroke-fg" : "stroke-muted"} />
            </g>
          ))}
        </g>
      ))}
      <T x={14} y={172} tone="faint" size={9.5}>
        sha256
      </T>
      {Array.from({ length: 22 }, (_, i) => (
        <Dither key={i} x={58 + i * 11.3} y={162} width={8} height={12} rx={1.5} level={([75, 25, 50, 75, 50, 25, 75, 50] as const)[(i * 3) % 8]} />
      ))}
      <T x={14} y={192} tone="muted" size={9.5}>
        every build checksummed
      </T>
    </Art>
  );
}

const SPANS = [
  { name: "GET /checkout", x: 104, w: 202, level: 25 },
  { name: "auth", x: 112, w: 38, level: 50 },
  { name: "orders.create", x: 154, w: 104, level: 50 },
  { name: "pg insert", x: 162, w: 52, level: 75 },
  { name: "publish", x: 262, w: 38, level: 50 },
] as const;

/** OpenTelemetry traces in Search: a trace's spans as rows, one span's event as a mark, in the traces index. */
export function TracesArt() {
  return (
    <Art>
      <T x={14} y={20} tone="muted" size={10.5}>
        traces-*.otel-*
      </T>
      <T x={306} y={20} tone="faint" anchor="end">
        212 ms
      </T>
      {SPANS.map((s, i) => {
        const y = 36 + i * 25;
        const hit = i === 3;
        return (
          <g key={s.name}>
            <T x={i === 0 ? 14 : 22} y={y + 11.5} tone={hit ? "fg" : "muted"} size={9.5}>
              {s.name}
            </T>
            <Dither x={s.x} y={y} width={s.w} height={15} rx={3} level={s.level} />
            <rect x={s.x} y={y} width={s.w} height={15} rx={3} className={hit ? "stroke-muted" : "stroke-line-2"} />
          </g>
        );
      })}
      <path d="M188,126 V174" className="stroke-accent" strokeDasharray="2 3" />
      <Beacon cx={188} cy={118} r={2.4} />
      <path d="M104,174 H306" className="stroke-line-2" />
      {[130, 188, 232, 282].map((x) => (
        <path key={x} d={`M${x},170 V178`} className={x === 188 ? "stroke-accent" : "stroke-subtle"} />
      ))}
      <T x={14} y={178} tone="faint" size={9.5}>
        events
      </T>
      <T x={104} y={194} tone="faint" size={9.5}>
        as log lines
      </T>
    </Art>
  );
}

const KEYS = [
  { key: "STRIPE_KEY", reach: true },
  { key: "STRIPE_WEBHOOK_SECRET", reach: true },
  { key: "DATABASE_URL", reach: false },
  { key: "SESSION_SECRET", reach: false },
  { key: "SENTRY_DSN", reach: false },
];

/** Key-scoped rules: a grant that reaches the keys it names, and nothing else in the environment. */
export function KeyRulesArt() {
  return (
    <Art>
      <rect x={14} y={62} width={84} height={40} rx={9} className="stroke-muted" />
      <T x={56} y={80} tone="fg" anchor="middle" size={10}>
        grant: ci
      </T>
      <T x={56} y={93} tone="faint" anchor="middle" size={9.5}>
        2 keys
      </T>
      <T x={14} y={18} tone="faint">
        production
      </T>
      {KEYS.map((k, i) => {
        const y = 22 + i * 32;
        return (
          <g key={k.key}>
            {k.reach && <path d={`M98,82 C116,82 114,${y + 12} 132,${y + 12}`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.5 + i * 0.3, 7)} />}
            <rect x={132} y={y} width={174} height={24} rx={7} className={k.reach ? "stroke-subtle" : "stroke-line-2"} strokeDasharray={k.reach ? undefined : "3 3"} />
            <T x={143} y={y + 15.5} tone={k.reach ? "fg" : "faint"} size={9.5}>
              {k.key}
            </T>
            {k.reach ? <Check cx={292} cy={y + 12} s={2.8} className="stroke-fg" /> : <Cross cx={292} cy={y + 12} s={2.4} />}
          </g>
        );
      })}
      <T x={14} y={192} tone="faint" size={9.5}>
        the rest: out of reach
      </T>
    </Art>
  );
}
