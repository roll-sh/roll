import { useReducedMotion } from "@roll/brand";
import { useEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { useSeen } from "./in-view";

/** Fades and lifts its children in the first time they scroll into view. */
export function Reveal({ children, delay = 0, className }: { children: ReactNode; delay?: number; className?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const seen = useSeen(ref);
  const reduced = useReducedMotion();
  const shown = seen || reduced;
  return (
    <div
      ref={ref}
      className={className}
      style={{
        opacity: shown ? 1 : 0,
        transform: shown ? "none" : "translateY(14px)",
        transition: `opacity 700ms cubic-bezier(0.23, 1, 0.32, 1) ${delay}ms, transform 700ms cubic-bezier(0.23, 1, 0.32, 1) ${delay}ms`,
      }}
    >
      {children}
    </div>
  );
}

/**
 * A measured number that counts up once it's seen, keeping its format: "1,700", "0.66 s", "1.1M",
 * "~500", "8×". The text is the real value from the start (for readers, crawlers and no-JS).
 */
export function CountUp({ value, className }: { value: string; className?: string }) {
  const ref = useRef<HTMLSpanElement>(null);
  const seen = useSeen(ref);
  const reduced = useReducedMotion();
  const [text, setText] = useState(value);
  useEffect(() => {
    const m = value.match(/^([^\d]*)([\d,]*\.?\d+)(.*)$/);
    if (!m || !seen || reduced) return setText(value);
    const [, pre, num, post] = m;
    const target = Number(num.replace(/,/g, ""));
    const decimals = num.includes(".") ? num.split(".")[1].length : 0;
    const commas = num.includes(",");
    const t0 = performance.now();
    let raf = 0;
    const step = (now: number) => {
      const t = Math.min(1, (now - t0) / 1400);
      const v = target * (1 - (1 - t) ** 4);
      const s = commas ? Math.round(v).toLocaleString("en-US") : v.toFixed(decimals);
      setText(pre + s + post);
      if (t < 1) raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf);
  }, [value, seen, reduced]);
  return (
    <span ref={ref} className={className} aria-label={value}>
      {text}
    </span>
  );
}

/**
 * A panel the cursor lights: the dot grid and the border brighten in a circle around the pointer,
 * the same gesture as the DotField, without a canvas per card.
 */
export function Spotlight({ children, className, style }: { children: ReactNode; className?: string; style?: CSSProperties }) {
  const ref = useRef<HTMLDivElement>(null);
  const onMove = (e: React.PointerEvent) => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    el.style.setProperty("--mx", `${e.clientX - r.left}px`);
    el.style.setProperty("--my", `${e.clientY - r.top}px`);
  };
  const mask = "radial-gradient(220px circle at var(--mx) var(--my), black, transparent)";
  return (
    <div ref={ref} onPointerMove={onMove} className={`group/spot relative isolate ${className ?? ""}`} style={style}>
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0 -z-10 rounded-[inherit] bg-[radial-gradient(var(--fg)_1px,transparent_1.3px)] [background-size:14px_14px] opacity-0 transition-opacity duration-300 group-hover/spot:opacity-[0.16]"
        style={{ maskImage: mask, WebkitMaskImage: mask }}
      />
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0 rounded-[inherit] border border-fg/40 opacity-0 transition-opacity duration-300 group-hover/spot:opacity-100"
        style={{ maskImage: mask, WebkitMaskImage: mask }}
      />
      {children}
    </div>
  );
}
