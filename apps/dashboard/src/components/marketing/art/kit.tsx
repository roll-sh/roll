import type { LucideIcon } from "lucide-react";
import { createContext, useContext, useId, type CSSProperties, type ReactNode } from "react";

/** The id prefix of the drawing's own halftone patterns. */
const Tones = createContext("");
const LEVELS = { 25: [[0, 0]], 50: [[0, 0], [3, 3]], 75: [[0, 0], [3, 3], [3, 0]] } as const;

/**
 * The bento's line art: one drawing per product, saying what it does at a glance. They're
 * concepts, never a picture of a dashboard page (those are real screenshots). Every drawing is
 * 320×200, hairlines in the theme's own inks (`stroke-line-2` faint, `stroke-subtle` mid,
 * `stroke-fg` bright) with the accent on one thing at most, and solid areas as an ordered dither
 * (`Dither`), the same halftone as the closing's landscape. What moves carries `data-a`, which
 * globals.css runs only while the card is on screen and never under reduced motion.
 */
export function Art({ children, w = 320, h = 200 }: { children: ReactNode; /** A bigger sheet, for a product's own page (art/hero). */ w?: number; h?: number }) {
  const id = useId().replace(/[^a-zA-Z0-9]/g, "");
  return (
    <svg viewBox={`0 0 ${w} ${h}`} fill="none" aria-hidden className="block h-auto w-full overflow-visible">
      <defs>
        {Object.entries(LEVELS).map(([level, dots]) => (
          <pattern key={level} id={`${id}t${level}`} width={6} height={6} patternUnits="userSpaceOnUse">
            {dots.map(([x, y]) => (
              <rect key={`${x}${y}`} x={x} y={y} width={1.7} height={1.7} className="fill-fg" />
            ))}
          </pattern>
        ))}
      </defs>
      <Tones.Provider value={id}>{children}</Tones.Provider>
    </svg>
  );
}

/** An area filled with halftone dots: a quarter, half or three quarters of them on. */
export function Dither({
  level = 50,
  d,
  rx,
  ...rest
}: {
  level?: keyof typeof LEVELS;
  /** A path; without it, a rectangle. */
  d?: string;
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  rx?: number;
  opacity?: number;
  className?: string;
}) {
  const fill = `url(#${useContext(Tones)}t${level})`;
  return d ? <path d={d} fill={fill} {...rest} /> : <rect rx={rx} fill={fill} {...rest} />;
}

const TONE = { faint: "fill-subtle", subtle: "fill-subtle", muted: "fill-muted", fg: "fill-fg", accent: "fill-accent" };

/** A mono label. */
export function T({
  x,
  y,
  children,
  tone = "subtle",
  size = 10,
  anchor,
}: {
  x: number;
  y: number;
  children: ReactNode;
  tone?: keyof typeof TONE;
  size?: number;
  anchor?: "middle" | "end";
}) {
  return (
    <text x={x} y={y} fontSize={size} textAnchor={anchor} className={`font-mono ${TONE[tone]}`}>
      {children}
    </text>
  );
}

/** A product's own icon inside a drawing, centred on (cx, cy). */
export function Glyph({ icon: Icon, cx, cy, size = 14, className = "text-muted" }: { icon: LucideIcon; cx: number; cy: number; size?: number; className?: string }) {
  return <Icon x={cx - size / 2} y={cy - size / 2} width={size} height={size} strokeWidth={1.5} className={className} />;
}

/** A pill with a mono label in it: a client, a command, a name. `w` is its width; the label is centred. */
export function Chip({
  x,
  y,
  w,
  h = 22,
  children,
  tone = "muted",
  line = "stroke-line-2",
  size = 9.5,
  fill = "",
}: {
  x: number;
  y: number;
  w: number;
  h?: number;
  children: ReactNode;
  tone?: keyof typeof TONE;
  line?: string;
  size?: number;
  fill?: string;
}) {
  return (
    <>
      <rect x={x} y={y} width={w} height={h} rx={h / 2} className={`${fill} ${line}`} />
      <T x={x + w / 2} y={y + h / 2 + size * 0.36} tone={tone} size={size} anchor="middle">
        {children}
      </T>
    </>
  );
}

/** A rounded square with an icon in it: a resource, a machine, a product. */
export function Tile({ cx, cy, s = 30, icon, line = "stroke-line-2", ink = "text-muted", dashed = false }: { cx: number; cy: number; s?: number; icon: LucideIcon; line?: string; ink?: string; dashed?: boolean }) {
  return (
    <>
      <rect x={cx - s / 2} y={cy - s / 2} width={s} height={s} rx={s * 0.27} strokeDasharray={dashed ? "3 3" : undefined} className={`fill-panel ${line}`} />
      <Glyph icon={icon} cx={cx} cy={cy} size={s * 0.47} className={ink} />
    </>
  );
}

/**
 * Dashes marching along a line (set the same `strokeDasharray`, whose period is `period`).
 * `scaled` lets the stroke grow with the drawing (a row of records) instead of staying a hairline.
 */
export const flow = (seconds: number, period: number, { reverse = false, scaled = false } = {}) => ({
  "data-a": "",
  style: {
    animation: `art-flow ${seconds}s linear infinite${reverse ? " reverse" : ""}`,
    "--flow": -period,
    ...(scaled && { vectorEffect: "none" }),
  } as CSSProperties,
});

/** A slow breath: something working, or waiting. */
export const pulse = (seconds = 2.4, delay = 0) => ({
  "data-a": "",
  style: { animation: `art-pulse ${seconds}s ease-in-out ${delay}s infinite` } as CSSProperties,
});

/** The drawing's one accent: a dot with a ring that spreads from it. */
export function Beacon({ cx, cy, r = 3 }: { cx: number; cy: number; r?: number }) {
  return (
    <>
      <circle
        cx={cx}
        cy={cy}
        r={r * 3.2}
        className="stroke-accent"
        data-a=""
        style={{ animation: "art-ping 2.4s ease-out infinite", transformBox: "fill-box", transformOrigin: "center", opacity: 0 }}
      />
      <circle cx={cx} cy={cy} r={r} className="fill-accent" />
    </>
  );
}

/** A small tick: done. */
export function Check({ cx, cy, s = 3, className = "stroke-fg" }: { cx: number; cy: number; s?: number; className?: string }) {
  return <path d={`M${cx - s},${cy} l${s * 0.75},${s * 0.8} l${s * 1.35},-${s * 1.6}`} className={className} strokeLinecap="round" strokeLinejoin="round" />;
}

/** A small cross: dropped, refused, failed. */
export function Cross({ cx, cy, s = 3, className = "stroke-subtle" }: { cx: number; cy: number; s?: number; className?: string }) {
  return <path d={`M${cx - s},${cy - s} l${2 * s},${2 * s} M${cx + s},${cy - s} l${-2 * s},${2 * s}`} className={className} strokeLinecap="round" />;
}
