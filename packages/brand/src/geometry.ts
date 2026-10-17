// Pure geometry for the roll mark: a globe of nodes whose dots shrink toward the rim.
// Self-contained (no imports) so scripts can run it directly with Node's type stripping.

export interface Node {
  /** Home position in unit space, [-1, 1] on both axes. */
  x: number;
  y: number;
  /** Distance from centre, 0..1. */
  d: number;
  /** Base radius in unit space. */
  r: number;
  /** Row index from the top, used for staggered animations. */
  row: number;
}

export function globeNodes(step: number): Node[] {
  const nodes: Node[] = [];
  const n = Math.floor(1 / step + 1e-9);
  const k = step * 0.43;
  for (let j = -n; j <= n; j++) {
    const y = j * step;
    for (let i = -n; i <= n; i++) {
      const x = i * step;
      const d = Math.hypot(x, y);
      // Trim lone tips on the axes so the silhouette reads as a circle, not a diamond.
      if (d > 1 - step * 0.3) continue;
      const r = k * (1 - 0.55 * d * d);
      nodes.push({ x, y, d, r, row: j + n });
    }
  }
  return nodes;
}

/** Where each node sits in the "horizon" formation: a dotted half-globe rising over a line. */
export function horizonTarget(node: Node): { x: number; y: number; scale: number } {
  if (node.y < 0) return { x: node.x, y: node.y + 0.32, scale: 1 };
  return { x: node.x * 1.04, y: 0.62, scale: 0.72 };
}

export interface MarkSvgOptions {
  step?: number;
  color?: string;
  /** Light one node in this colour (the "region" signature). */
  accent?: string;
  size?: number;
  formation?: "globe" | "horizon";
}

const ACCENT_POS = { x: 0.32, y: -0.32 };

/** The accent node fills most of its cell so it stays visible at favicon sizes. */
export function accentRadius(node: Node, step: number): number {
  return Math.max(node.r * 1.25, step * 0.44);
}

export function accentIndex(nodes: Node[]): number {
  let best = 0;
  let bestD = Infinity;
  nodes.forEach((n, i) => {
    const d = Math.hypot(n.x - ACCENT_POS.x, n.y - ACCENT_POS.y);
    if (d < bestD) {
      bestD = d;
      best = i;
    }
  });
  return best;
}

export function markSvg({
  step = 0.25,
  color = "#fafafa",
  accent,
  size = 100,
  formation = "globe",
}: MarkSvgOptions = {}): string {
  const nodes = globeNodes(step);
  const ai = accent ? accentIndex(nodes) : -1;
  const f = (v: number) => Math.round(v * 1000) / 1000;
  const seen = new Set<string>();
  const circles = nodes
    .map((n, i) => {
      const t = formation === "horizon" ? horizonTarget(n) : { x: n.x, y: n.y, scale: 1 };
      const key = `${f(t.x)}:${f(t.y)}`;
      if (seen.has(key)) return "";
      seen.add(key);
      const fill = i === ai ? ` fill="${accent}"` : "";
      const r = i === ai ? accentRadius(n, step) : n.r * t.scale;
      return `<circle cx="${f(50 + t.x * 46)}" cy="${f(50 + t.y * 46)}" r="${f(r * 46)}"${fill}/>`;
    })
    .join("");
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 100 100" fill="${color}">${circles}</svg>`;
}

// ── Horizon (E7): a banded half-globe rising over a line. Used as the profile picture. ──

export interface Band {
  /** SVG path in the 0..100 box. */
  d: string;
  /** Vertical extent, for filling the band with dots. */
  y1: number;
  y2: number;
  /** Horizontal half-width at a given y. */
  halfWidth: (y: number) => number;
  cx: number;
}

const HZ = { cx: 50, cy: 64, r: 44, bands: [[19, 31], [35, 47], [51, 64]] as const, line: { x: 6, y: 72, w: 88, h: 7 } };

export function horizonBands(): Band[] {
  const { cx, cy, r } = HZ;
  const f = (v: number) => Math.round(v * 100) / 100;
  const hw = (y: number) => Math.sqrt(Math.max(0, r * r - (y - cy) ** 2));
  const bands: Band[] = HZ.bands.map(([y1, y2]) => {
    const top = y1 <= cy - r + 0.5;
    const a = hw(y1);
    const b = hw(y2);
    const d = top
      ? `M${f(cx - b)} ${y2}A${r} ${r} 0 0 1 ${f(cx + b)} ${y2}Z`
      : `M${f(cx - a)} ${y1}H${f(cx + a)}A${r} ${r} 0 0 1 ${f(cx + b)} ${y2}H${f(cx - b)}A${r} ${r} 0 0 1 ${f(cx - a)} ${y1}Z`;
    return { d, y1: Math.max(y1, cy - r), y2, halfWidth: hw, cx };
  });
  const { x, y, w, h } = HZ.line;
  bands.push({
    d: `M${x + h / 2} ${y}H${x + w - h / 2}A${h / 2} ${h / 2} 0 0 1 ${x + w - h / 2} ${y + h}H${x + h / 2}A${h / 2} ${h / 2} 0 0 1 ${x + h / 2} ${y}Z`,
    y1: y,
    y2: y + h,
    halfWidth: () => w / 2 - 1,
    cx: 50,
  });
  return bands;
}

export function horizonSvg({ color = "#fafafa", size = 100 }: { color?: string; size?: number } = {}): string {
  const paths = horizonBands().map((b) => `<path d="${b.d}"/>`).join("");
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 100 100" fill="${color}">${paths}</svg>`;
}

/** Square profile picture: Horizon centred on a solid tile (platforms apply their own crop). */
export function avatarSvg({ background = "#0a0a0a", color = "#fafafa", size = 1024 } = {}): string {
  const paths = horizonBands().map((b) => `<path d="${b.d}"/>`).join("");
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 100 100"><rect width="100" height="100" fill="${background}"/><g fill="${color}" transform="translate(50 50) scale(.6) translate(-50 -49.5)">${paths}</g></svg>`;
}
