import { Art, Beacon, Check, Chip, Cross, Dither, T, flow, pulse } from "./kit";

/** The pictures on the messaging products' own pages (see hero-data.tsx for the rules). */
const W = 560;
const H = 400;

const PRODUCERS = [
  { name: "checkout", y: 66, to: [60, 104] },
  { name: "orders-api", y: 118, to: [148] },
  { name: "curl -d …", y: 170, to: [192] },
];
const CONSUMERS = [
  { name: "billing-1", y: 70, from: [60, 104] },
  { name: "billing-2", y: 158, from: [148, 192] },
];

/** Streams: producers, a topic's partitions, a consumer group reading them, and sealed segments going to object storage. */
export function StreamsHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 30)">
        <T x={16} y={30} tone="faint" size={8.5}>
          producers
        </T>
        <T x={150} y={30} tone="faint" size={8.5}>
          topic orders
        </T>
        <T x={544} y={30} tone="faint" anchor="end" size={8.5}>
          group billing
        </T>
        {PRODUCERS.map((p, i) => (
          <g key={p.name}>
            <Chip x={16} y={p.y} w={92} h={24}>
              {p.name}
            </Chip>
            {p.to.map((y) => (
              <path key={y} d={`M108,${p.y + 12} C130,${p.y + 12} 128,${y} 150,${y}`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.3, 7)} />
            ))}
          </g>
        ))}
        {[0, 1, 2, 3].map((i) => {
          const y = 46 + i * 44;
          return (
            <g key={i}>
              <rect x={150} y={y} width={250} height={28} rx={7} className="fill-panel stroke-line-2" />
              <T x={160} y={y + 18}>
                p{i}
              </T>
              <path
                d={`M184,${y + 14} H396`}
                strokeWidth={12}
                strokeDasharray="9 6"
                className={i === 1 ? "stroke-subtle" : "stroke-subtle/55"}
                {...flow(1.6 + i * 0.4, 15, { reverse: true, scaled: true })}
              />
            </g>
          );
        })}
        {CONSUMERS.map((c, i) => (
          <g key={c.name}>
            {c.from.map((y) => (
              <path key={y} d={`M400,${y} C424,${y} 422,${c.y + 12} 446,${c.y + 12}`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.3 + i * 0.3, 7)} />
            ))}
            <Chip x={446} y={c.y} w={98} h={24} tone="fg" line="stroke-subtle">
              {c.name}
            </Chip>
          </g>
        ))}
        <path d="M322,84 V122" className="stroke-accent" />
        <path d="M317,84 h10 l-5,6 z" className="fill-accent" />
        <T x={332} y={86} tone="accent" size={8.5}>
          offset 1042
        </T>

        <path d="M200,224 V284" className="stroke-subtle" strokeDasharray="2 3" {...flow(1.6, 5)} />
        <T x={210} y={258} tone="faint" size={8.5}>
          sealed segments
        </T>
        <T x={16} y={306} tone="faint" size={8.5}>
          object storage
        </T>
        {Array.from({ length: 10 }, (_, i) => (
          <Dither key={i} x={150 + i * 25} y={292} width={19} height={18} rx={4} level={i < 4 ? 75 : i < 7 ? 50 : 25} />
        ))}
        <T x={544} y={306} tone="faint" anchor="end" size={8.5}>
          bottomless
        </T>
      </g>
    </Art>
  );
}

const PROTOCOLS = [
  { name: "AMQP", y: 70 },
  { name: "MQTT", y: 140 },
  { name: "STOMP", y: 210 },
];
const QUEUES = [
  { name: "billing", key: "order.paid", y: 71, waiting: 5 },
  { name: "email", key: "order.*", y: 151, waiting: 2 },
  { name: "audit", key: "#", y: 231, waiting: 7 },
];

/** Queues: three protocols into one vhost, an exchange routing by key to queues, and consumers taking from them. */
export function QueuesHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 40)">
        <rect x={126} y={26} width={308} height={246} rx={14} strokeDasharray="4 4" className="stroke-line-2" />
        <T x={138} y={18} tone="faint" size={8.5}>
          vhost shop
        </T>
        <T x={434} y={290} tone="faint" anchor="end" size={8.5}>
          asleep until something connects
        </T>
        {PROTOCOLS.map((p, i) => (
          <g key={p.name}>
            <Chip x={16} y={p.y} w={66} h={24}>
              {p.name}
            </Chip>
            <path d={`M82,${p.y + 12} C118,${p.y + 12} 118,151 154,151`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.3, 7)} />
          </g>
        ))}
        <path d="M170,135 L186,151 L170,167 L154,151 Z" className="fill-panel stroke-fg" />
        <T x={170} y={186} tone="faint" anchor="middle" size={8.5}>
          exchange
        </T>
        {QUEUES.map((q, i) => (
          <g key={q.name}>
            <path d={`M186,151 C232,151 232,${q.y} 278,${q.y}`} className="stroke-subtle" />
            <T x={278} y={q.y - 20} tone="muted">
              {q.name}
            </T>
            <T x={410} y={q.y - 20} tone="faint" anchor="end" size={8.5}>
              {q.key}
            </T>
            {Array.from({ length: 9 }, (_, k) => {
              const x = 278 + k * 15;
              return (
                <g key={k}>
                  <rect x={x} y={q.y - 11} width={12} height={22} rx={3} className={k >= 9 - q.waiting ? "fill-subtle/70 stroke-subtle" : "stroke-line-2"} />
                </g>
              );
            })}
            <path d={`M410,${q.y} H466`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.2 + i * 0.4, 7)} />
            <circle cx={480} cy={q.y} r={13} className="fill-panel stroke-subtle" />
            <T x={502} y={q.y + 3} tone={i === 1 ? "accent" : "faint"} size={8.5}>
              {i === 1 ? "deliver" : "ack"}
            </T>
          </g>
        ))}
        <Check cx={480} cy={71} />
        <Beacon cx={480} cy={151} />
        <Check cx={480} cy={231} />
      </g>
    </Art>
  );
}

const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const STEPS = [
  { x: 150, name: "load-user" },
  { x: 270, name: "send-welcome" },
  { x: 390, name: "sleep 1d" },
  { x: 510, name: "activation" },
];

/** Tasks: the three things it runs, each on its own line of time: a message with its retries, a schedule, a workflow's steps. */
export function TasksHero() {
  return (
    <Art w={W} h={H}>
      <g transform="translate(0 26)">
        <T x={16} y={78} tone="muted" size={10.5}>
          message
        </T>
        <path d="M120,74 H230" className="stroke-subtle" strokeDasharray="2 4" />
        <path d="M230,74 H390" className="stroke-subtle" strokeDasharray="2 4" />
        <path d="M390,74 H544" className="stroke-fg" />
        <circle cx={120} cy={74} r={3} className="fill-fg" />
        <T x={120} y={52} tone="muted" anchor="middle" size={8.5}>
          publish
        </T>
        <T x={175} y={66} tone="faint" anchor="middle" size={8.5}>
          delay 30s
        </T>
        <circle cx={230} cy={74} r={11} className="fill-panel stroke-subtle" />
        <Cross cx={230} cy={74} />
        <T x={230} y={52} tone="muted" anchor="middle" size={8.5}>
          attempt 1
        </T>
        <T x={230} y={102} tone="faint" anchor="middle" size={8.5}>
          503
        </T>
        <T x={310} y={66} tone="faint" anchor="middle" size={8.5}>
          backoff
        </T>
        <circle cx={390} cy={74} r={11} className="fill-panel stroke-fg" />
        <Check cx={390} cy={74} />
        <T x={390} y={52} tone="fg" anchor="middle" size={8.5}>
          attempt 2
        </T>
        <T x={390} y={102} tone="faint" anchor="middle" size={8.5}>
          200
        </T>
        <T x={544} y={66} tone="faint" anchor="end" size={8.5}>
          delivered, signed
        </T>

        <path d="M16,126 H544" className="stroke-line" />
        <T x={16} y={192} tone="muted" size={10.5}>
          schedule
        </T>
        <Chip x={110} y={138} w={132} tone="fg" line="stroke-subtle">
          0 9 * * MON-FRI
        </Chip>
        <T x={544} y={153} tone="faint" anchor="end" size={8.5}>
          Europe/Budapest
        </T>
        <path d="M120,188 H544" className="stroke-line-2" />
        {DAYS.map((d, i) => {
          const x = 140 + i * 64;
          const off = i > 4;
          return (
            <g key={d}>
              <circle cx={x} cy={188} r={off ? 3 : 4.5} strokeDasharray={off ? "2 2" : undefined} className={off ? "fill-panel stroke-line-2" : i < 2 ? "fill-fg" : "fill-panel stroke-subtle"} />
              <T x={x} y={212} tone={off ? "faint" : i < 2 ? "muted" : "subtle"} anchor="middle" size={8.5}>
                {d}
              </T>
            </g>
          );
        })}
        <circle cx={268} cy={188} r={9} className="stroke-fg" />
        <T x={268} y={172} tone="fg" anchor="middle" size={8.5}>
          next · 09:00
        </T>

        <path d="M16,238 H544" className="stroke-line" />
        <T x={16} y={292} tone="muted" size={10.5}>
          workflow
        </T>
        <path d="M162,288 H258 M282,288 H378" className="stroke-subtle" />
        <path d="M402,288 H498" className="stroke-line-2" strokeDasharray="3 3" />
        {STEPS.map((s, i) => (
          <g key={s.name}>
            <circle cx={s.x} cy={288} r={12} strokeDasharray={i === 3 ? "3 3" : undefined} className={i < 2 ? "fill-panel stroke-fg" : i === 2 ? "fill-panel stroke-accent" : "fill-panel stroke-line-2"} />
            {i < 2 && <Check cx={s.x} cy={288} />}
            {i === 2 && <circle cx={s.x} cy={288} r={3.4} className="fill-accent" {...pulse(1.8)} />}
            <T x={s.x} y={320} tone={i < 2 ? "muted" : i === 2 ? "fg" : "faint"} anchor="middle" size={8.5}>
              {s.name}
            </T>
          </g>
        ))}
        <T x={390} y={264} tone="accent" anchor="middle" size={8.5}>
          asleep, kept
        </T>
      </g>
    </Art>
  );
}
