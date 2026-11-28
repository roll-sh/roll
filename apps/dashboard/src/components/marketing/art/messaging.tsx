import { Art, Beacon, Check, Dither, T, flow, pulse } from "./kit";

/** Streams: records moving down a topic's partitions, a consumer's place in one, old segments going to object storage. */
export function StreamsArt() {
  return (
    <Art>
      <T x={16} y={18} tone="faint">
        topic orders
      </T>
      {[0, 1, 2].map((i) => {
        const y = 30 + i * 38;
        return (
          <g key={i}>
            <T x={16} y={y + 15}>
              p{i}
            </T>
            <rect x={40} y={y} width={300} height={22} rx={5} className="stroke-line-2" />
            <path
              d={`M47,${y + 11} H340`}
              strokeWidth={10}
              strokeDasharray="8 5"
              className={i === 1 ? "stroke-subtle" : "stroke-subtle/55"}
              {...flow(1.5 + i * 0.5, 13, { reverse: true, scaled: true })}
            />
          </g>
        );
      })}
      <path d="M214,60 V98" className="stroke-accent" />
      <path d="M210,60 h8 l-4,5 z" className="fill-accent" />
      <T x={222} y={64} tone="accent" size={8.5}>
        offset
      </T>

      <path d="M62,128 V158" className="stroke-subtle" strokeDasharray="2 3" {...flow(1.6, 5)} />
      <T x={16} y={176} tone="faint">
        object storage
      </T>
      {[0, 1, 2, 3, 4, 5, 6, 7, 8].map((i) => (
        <Dither key={i} x={112 + i * 24} y={164} width={18} height={14} rx={3} level={i < 3 ? 75 : i < 6 ? 50 : 25} />
      ))}
    </Art>
  );
}

const PROTOCOLS = [
  { name: "AMQP", y: 50, d: "M56,50 C84,50 84,100 103,100" },
  { name: "MQTT", y: 100, d: "M56,100 H103" },
  { name: "STOMP", y: 150, d: "M60,150 C86,150 84,100 103,100" },
];
const QUEUES = [
  { y: 58, waiting: 4 },
  { y: 142, waiting: 2 },
];

/** Queues: publishers in three protocols, an exchange routing to queues, consumers taking from them. */
export function QueuesArt() {
  return (
    <Art>
      {PROTOCOLS.map((p, i) => (
        <g key={p.name}>
          <T x={16} y={p.y + 3} tone="muted">
            {p.name}
          </T>
          <path d={p.d} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.3, 7)} />
        </g>
      ))}
      <path d="M112,90 L122,100 L112,110 L102,100 Z" className="stroke-fg" />
      <T x={112} y={128} tone="faint" anchor="middle" size={8.5}>
        exchange
      </T>
      {QUEUES.map((q, i) => (
        <g key={q.y}>
          <path d={`M122,100 C142,100 138,${q.y} 156,${q.y}`} className="stroke-subtle" />
          {[0, 1, 2, 3, 4, 5].map((k) => {
            const x = 156 + k * 16;
            return (
              <g key={k}>
                {k >= 6 - q.waiting && <Dither x={x + 2} y={q.y - 9} width={8} height={18} level={75} />}
                <rect x={x} y={q.y - 11} width={12} height={22} rx={3} className="stroke-line-2" />
              </g>
            );
          })}
          <path d={`M252,${q.y} H274`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.2 + i * 0.4, 7)} />
          <circle cx={285} cy={q.y} r={10} className="stroke-subtle" />
        </g>
      ))}
      <Check cx={285} cy={58} />
      <Beacon cx={285} cy={142} />
    </Art>
  );
}

const STEPS = [
  { x: 40, name: "charge" },
  { x: 118, name: "email" },
  { x: 196, name: "ship" },
  { x: 274, name: "done" },
];

/** Tasks: a schedule that starts a workflow, whose steps are kept: two done, one running, one to go. */
export function TasksArt() {
  return (
    <Art>
      <circle cx={40} cy={46} r={22} className="stroke-subtle" />
      {Array.from({ length: 12 }, (_, i) => {
        const a = (i / 12) * Math.PI * 2;
        const r = i % 3 === 0 ? 16 : 18.5;
        // Rounded, so the server's and the browser's floats print the same path.
        const at = (v: number) => v.toFixed(2);
        return <path key={i} d={`M${at(40 + Math.sin(a) * r)},${at(46 - Math.cos(a) * r)} L${at(40 + Math.sin(a) * 20)},${at(46 - Math.cos(a) * 20)}`} className="stroke-line-2" />;
      })}
      <path d="M40,46 V31 M40,46 H29" className="stroke-fg" strokeLinecap="round" />
      <rect x={78} y={24} width={84} height={22} rx={6} className="stroke-line-2" />
      <T x={90} y={39} tone="fg" size={10}>
        0 9 * * 1
      </T>
      <T x={78} y={62} tone="faint">
        Mondays, 09:00
      </T>

      <path d="M40,68 V120" className="stroke-subtle" strokeDasharray="2 3" {...flow(1.6, 5)} />
      <path d="M50,132 H108 M128,132 H186" className="stroke-subtle" />
      <path d="M206,132 H264" className="stroke-line-2" strokeDasharray="3 3" />
      {STEPS.map((s, i) => (
        <g key={s.name}>
          <circle
            cx={s.x}
            cy={132}
            r={10}
            strokeDasharray={i === 3 ? "3 3" : undefined}
            className={i < 2 ? "stroke-fg" : i === 2 ? "stroke-accent" : "stroke-line-2"}
          />
          {i < 2 && <Check cx={s.x} cy={132} />}
          {i === 2 && <circle cx={s.x} cy={132} r={3} className="fill-accent" {...pulse(1.8)} />}
          <T x={s.x} y={160} tone={i < 2 ? "muted" : i === 2 ? "fg" : "faint"} anchor="middle">
            {s.name}
          </T>
        </g>
      ))}
      <path d="M187,118 A13,13 0 1 1 205,118" className="stroke-subtle" strokeDasharray="2 3" />
      <path d="M205,118 l1,-6 M205,118 l-6,-1" className="stroke-subtle" strokeLinecap="round" />
      <T x={196} y={96} tone="faint" anchor="middle" size={8.5}>
        retry
      </T>
    </Art>
  );
}
