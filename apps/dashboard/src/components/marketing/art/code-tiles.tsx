import { Art, Check, Cross, T, flow } from "./kit";

const BATCH = [
  { id: "chg_25e3", ok: true },
  { id: "chg_55b4", ok: false },
  { id: "chg_3dcc", ok: true },
];

/** A batch in the merge queue: the change that fails drops out, and the rest land on main. */
export function DropOutArt() {
  return (
    <Art>
      <T x={14} y={18} tone="faint">
        batch
      </T>
      <T x={306} y={18} tone="faint" anchor="end">
        main
      </T>
      {BATCH.map((c, i) => {
        const y = 34 + i * 42;
        return (
          <g key={c.id}>
            <rect x={14} y={y} width={122} height={30} rx={8} className={c.ok ? "stroke-subtle" : "stroke-line-2"} strokeDasharray={c.ok ? undefined : "3 3"} />
            <T x={26} y={y + 19} tone={c.ok ? "fg" : "faint"} size={10}>
              {c.id}
            </T>
            {c.ok ? <Check cx={120} cy={y + 15} s={2.8} className="stroke-fg" /> : <Cross cx={120} cy={y + 15} s={2.6} />}
            {c.ok ? (
              <path d={`M136,${y + 15} C180,${y + 15} 180,91 222,91`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.3 + i * 0.2, 7)} />
            ) : (
              <path d={`M136,${y + 15} C164,${y + 15} 160,170 190,170`} className="stroke-line-2" strokeDasharray="2 4" />
            )}
          </g>
        );
      })}
      <path d="M222,91 H306" className="stroke-fg" />
      {[244, 272].map((x) => (
        <circle key={x} cx={x} cy={91} r={2.6} className="fill-fg" />
      ))}
      <circle cx={303} cy={91} r={3.4} className="fill-fg" />
      <T x={196} y={173} tone="faint" size={9.5}>
        dropped out
      </T>
    </Art>
  );
}

const COMMITS = [
  { change: "chg_9a41", commit: "3f9c2e1" },
  { change: "chg_0dfb", commit: "b71d04a" },
  { change: "chg_f612", commit: "e2a88c6" },
];

/** git, derived: each changeset on roll has the commit git sees, with the same id everywhere. */
export function GitArt() {
  return (
    <Art>
      <T x={14} y={18} tone="faint">
        on roll
      </T>
      <T x={306} y={18} tone="faint" anchor="end">
        what git sees
      </T>
      {COMMITS.map((c, i) => {
        const y = 34 + i * 40;
        return (
          <g key={c.change}>
            <rect x={14} y={y} width={108} height={28} rx={8} className="stroke-line-2" />
            <T x={26} y={y + 18} tone="muted" size={10}>
              {c.change}
            </T>
            <path d={`M122,${y + 14} H198`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.4 + i * 0.2, 7)} />
            <rect x={198} y={y} width={108} height={28} rx={8} className="stroke-subtle" />
            <T x={210} y={y + 18} tone="fg" size={10}>
              {c.commit}
            </T>
          </g>
        );
      })}
      <T x={14} y={178} tone="faint" size={10}>
        $
      </T>
      <T x={28} y={178} tone="fg" size={10}>
        git clone https://roll.sh/acme/web
      </T>
    </Art>
  );
}
