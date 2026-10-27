import { languageShares } from "@/lib/code-api";

/** Languages by bytes: one monochrome bar (shades of the foreground, largest darkest) and a legend. */
export function Languages({ languages }: { languages: Record<string, number> }) {
  const all = languageShares(languages);
  if (!all.length) return null;
  const top = all.slice(0, 6);
  const rest = all.slice(6).reduce((s, l) => s + l.share, 0);
  const rows = rest > 0 ? [...top, { name: "Other", bytes: 0, share: rest }] : top;
  const shade = (i: number) => `color-mix(in srgb, var(--fg) ${Math.max(14, 92 - i * 16)}%, var(--bg))`;
  return (
    <div>
      <div className="flex h-1.5 gap-px overflow-hidden rounded-full">
        {rows.map((l, i) => (
          <span key={l.name} style={{ width: `${l.share * 100}%`, background: shade(i) }} title={`${l.name} ${(l.share * 100).toFixed(1)}%`} />
        ))}
      </div>
      <ul className="mt-3 grid grid-cols-2 gap-x-3 gap-y-1.5 text-[12px]">
        {rows.map((l, i) => (
          <li key={l.name} className="flex min-w-0 items-center gap-1.5">
            <span className="size-2 shrink-0 rounded-full" style={{ background: shade(i) }} />
            <span className="truncate text-muted">{l.name}</span>
            <span className="ml-auto font-mono text-[11px] text-subtle tabular-nums">{(l.share * 100).toFixed(1)}%</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** Merges per day as a calm static line; the last day carries the accent dot. */
export function Sparkline({ values, width = 120, height = 28 }: { values: number[]; width?: number; height?: number }) {
  if (values.length < 2) return null;
  const max = Math.max(1, ...values);
  const step = width / (values.length - 1);
  const pts = values.map((v, i) => [i * step, height - 2 - (v / max) * (height - 4)] as const);
  const d = pts.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(1)},${y.toFixed(1)}`).join("");
  const [lx, ly] = pts[pts.length - 1];
  return (
    <svg width={width} height={height} viewBox={`0 0 ${width} ${height}`} className="overflow-visible" aria-label="Merges per day, last 30 days" role="img">
      <path d={`${d}L${width},${height}L0,${height}Z`} fill="currentColor" opacity={0.06} />
      <path d={d} fill="none" stroke="currentColor" strokeWidth={1.25} strokeLinejoin="round" strokeLinecap="round" opacity={0.7} />
      <circle cx={lx} cy={ly} r={2} className="fill-accent" />
    </svg>
  );
}
