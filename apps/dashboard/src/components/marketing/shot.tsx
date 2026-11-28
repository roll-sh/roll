import { useReducedMotion } from "@roll/brand";
import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { useInView } from "./in-view";
import { SHOT_SIZE, SHOTS, type Focus, type ShotName } from "./shots";

/** Seconds the tour stays on a whole page, and on one part of it. */
const DWELL = { whole: 4.5, part: 6.5 };
/** The camera's move, in ms. */
const MOVE = 1000;
/** Room kept clear at the bottom of the picture for the callout laid over it. */
const CALLOUT = 148;
/** The frame's inner width from which the callout lies over the picture (below it: under the picture). */
const OVERLAY_FROM = 560;

/**
 * The hairline frame around a picture of the product. The picture is always the dashboard itself:
 * scripts/marketing-shots renders the app against fixtures, so it's the product 1:1.
 */
function Frame({ children }: { children: ReactNode }) {
  return (
    <figure className="rounded-[20px] border border-line-2 bg-panel-2/40 p-1.5 sm:p-2">
      <div className="relative overflow-hidden rounded-xl border border-line bg-bg">{children}</div>
    </figure>
  );
}

type Rect = { left: number; top: number; width: number; height: number };
type Camera = { x: number; y: number; s: number; ring: Rect | null };

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/**
 * Where the picture sits in a `vw`×`vh` window to look at `focus`: scaled so the part fits (never
 * past twice the dashboard's own size, where the render goes soft, and never so small its text
 * can't be read: then the window shows the part's top left), centred in what `reserve` leaves
 * free, and never showing past the picture's edge. No focus is the whole page, fitted to the width.
 */
export function camera(focus: Focus | undefined, vw: number, vh: number, reserve = 0): Camera {
  if (!focus || !vw || !vh) return { x: 0, y: 0, s: 1, ring: null };
  const k = vw / SHOT_SIZE.width;
  const [fx, fy, fw, fh] = focus;
  const free = vh - reserve;
  const fit = Math.min(vw / (fw * k), free / (fh * k)) * 0.88;
  const most = clamp(vw / 600, 1, 2) / k;
  const s = Math.max(1, clamp(fit, 0.8 / k, most));
  const w = fw * k * s;
  const h = fh * k * s;
  const pad = vw * 0.04;
  const x = clamp(w > vw - 2 * pad ? pad - fx * k * s : vw / 2 - (fx * k * s + w / 2), vw - vw * s, 0);
  const tall = SHOT_SIZE.height * k * s;
  const y = clamp(h > free - 2 * pad ? pad - fy * k * s : free / 2 - (fy * k * s + h / 2), Math.min(0, vh - tall), 0);
  return { x, y, s, ring: { left: x + fx * k * s, top: y + fy * k * s, width: w, height: h } };
}

const TOUR: ShotName[] = ["canvas", "postgres", "streams", "service", "code", "agents", "secrets", "search"];

/**
 * The dashboard, shown the way you'd show it to someone: a page at a time, and on each page a
 * camera that moves in on the parts worth a closer look, each with a line saying what it is. It
 * goes on by itself while it's on screen and the callout isn't being read; the tabs pick a page
 * and the callout's bars pick a stop. Under reduced motion nothing moves by itself and the camera
 * cuts. With one page (a product's own) there are no tabs.
 */
export function Tour({ pages = TOUR, className }: { pages?: ShotName[]; className?: string }) {
  const [at, setAt] = useState(0);
  const [stop, setStop] = useState(0);
  const [held, setHeld] = useState(false);
  // Pages opened so far: a page's picture is fetched when it's next, and kept once seen.
  const [seen, setSeen] = useState<ReadonlySet<number>>(() => new Set([0]));
  const [view, setView] = useState({ w: 0, h: 0 });
  const box = useRef<HTMLDivElement>(null);
  const win = useRef<HTMLDivElement>(null);
  const tabs = useRef<HTMLDivElement>(null);
  const inView = useInView(box);
  const reduced = useReducedMotion();
  const running = inView && !held;

  const shot = SHOTS[pages[at]];
  const beats = shot.beats;
  const beat = beats[stop];
  const overlay = view.w >= OVERLAY_FROM;
  const cam = camera(beat.focus, view.w, view.h, overlay ? CALLOUT : 0);
  const ring = cam.ring ?? { left: 0, top: 0, width: view.w, height: view.h };
  // The callout sits bottom left, unless what's in focus is there: then bottom right, or above it.
  const under = overlay && !!cam.ring && cam.ring.top + cam.ring.height > view.h - CALLOUT - 8;
  const place = !under || cam.ring!.left > 420 ? "sm:bottom-4 sm:left-4" : cam.ring!.left + cam.ring!.width < view.w - 420 ? "sm:right-4 sm:bottom-4" : "sm:top-4 sm:left-4";

  useEffect(() => {
    const el = win.current;
    if (!el) return;
    const measure = () => setView({ w: el.clientWidth, h: el.clientHeight });
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    measure();
    return () => ro.disconnect();
  }, []);

  // Keep the open tab visible in the row, which scrolls sideways on a phone.
  useEffect(() => {
    const row = tabs.current;
    const tab = row?.children[at] as HTMLElement | undefined;
    if (!row || !tab) return;
    const left = tab.offsetLeft - (row.clientWidth - tab.offsetWidth) / 2;
    row.scrollTo({ left, behavior: reduced ? "auto" : "smooth" });
  }, [at, reduced]);

  const open = (page: number) => {
    setAt(page);
    setStop(0);
    setSeen((was) => (was.has(page) ? was : new Set(was).add(page)));
  };
  const next = () => {
    if (stop + 1 < beats.length) setStop(stop + 1);
    else open((at + 1) % pages.length);
  };

  const onKey = (e: KeyboardEvent) => {
    const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const to = (at + step + pages.length) % pages.length;
    open(to);
    (tabs.current?.children[to] as HTMLElement | undefined)?.focus();
  };

  const move = reduced ? "none" : `transform ${MOVE}ms var(--shot-ease), opacity 500ms ease-out`;
  const follow = reduced
    ? "none"
    : `left ${MOVE}ms var(--shot-ease), top ${MOVE}ms var(--shot-ease), width ${MOVE}ms var(--shot-ease), height ${MOVE}ms var(--shot-ease), opacity 400ms ease-out`;

  return (
    <div ref={box} className={className}>
      {pages.length > 1 && (
        <div className="mb-4 flex justify-center">
          <div
            ref={tabs}
            role="tablist"
            aria-label="Pages of the dashboard"
            onKeyDown={onKey}
            className="flex max-w-full gap-0.5 overflow-x-auto rounded-full border border-line bg-panel/80 p-1 backdrop-blur [scrollbar-width:none]"
          >
            {pages.map((name, k) => {
              const s = SHOTS[name];
              const on = k === at;
              return (
                <button
                  key={name}
                  type="button"
                  role="tab"
                  id={`tour-tab-${name}`}
                  aria-selected={on}
                  aria-controls="tour-panel"
                  tabIndex={on ? 0 : -1}
                  onClick={() => open(k)}
                  className={`flex shrink-0 items-center gap-1.5 rounded-full px-3.5 py-1.5 text-[13px] transition-colors duration-200 ${
                    on ? "bg-panel-2 text-fg" : "text-muted hover:text-fg"
                  }`}
                >
                  <s.icon className="size-3.5" strokeWidth={1.5} />
                  {s.label}
                </button>
              );
            })}
          </div>
        </div>
      )}
      <Frame>
        <div
          id={pages.length > 1 ? "tour-panel" : undefined}
          role={pages.length > 1 ? "tabpanel" : undefined}
          aria-labelledby={pages.length > 1 ? `tour-tab-${pages[at]}` : undefined}
        >
          <div ref={win} className="relative aspect-[16/10] overflow-hidden lg:aspect-[16/9]">
            {pages.map((name, k) => {
              const s = SHOTS[name];
              const on = k === at;
              if (!seen.has(k) && k !== (at + 1) % pages.length) return null;
              return (
                <div
                  key={name}
                  className="absolute top-0 left-0 w-full origin-top-left"
                  style={{ transform: on ? `translate(${cam.x}px, ${cam.y}px) scale(${cam.s})` : "none", opacity: on ? 1 : 0, transition: move }}
                >
                  <img
                    src={s.src}
                    width={s.width}
                    height={s.height}
                    alt={on ? s.alt : ""}
                    aria-hidden={!on}
                    loading={k === 0 ? "eager" : "lazy"}
                    fetchPriority={k === 0 && pages.length > 1 ? "high" : undefined}
                    decoding="async"
                    className="block h-auto w-full"
                  />
                </div>
              );
            })}
            <div
              aria-hidden
              className="pointer-events-none absolute rounded-[10px] border border-accent/80 shadow-[0_0_0_200vmax_var(--shot-shade)]"
              style={{ ...ring, opacity: cam.ring ? 1 : 0, transition: follow }}
            />
          </div>
          <div
            onPointerEnter={(e) => e.pointerType === "mouse" && setHeld(true)}
            onPointerLeave={() => setHeld(false)}
            onFocus={() => setHeld(true)}
            onBlur={() => setHeld(false)}
            className={`border-t border-line bg-panel p-4 sm:absolute sm:w-[25rem] sm:rounded-xl sm:border sm:border-line-2 sm:bg-panel/85 sm:p-5 sm:backdrop-blur-md ${place}`}
          >
            <p className="flex items-center gap-2 font-mono text-[11px] text-subtle">
              <shot.icon className="size-3.5 text-muted" strokeWidth={1.5} />
              <span className="text-muted">{shot.label}</span>
              <span className="ml-auto tabular-nums">
                {stop + 1} / {beats.length}
              </span>
            </p>
            <div aria-live={running && !reduced ? "off" : "polite"} className="min-h-[5.5rem] sm:min-h-0">
              <p className="mt-2.5 text-[16px] font-medium tracking-[-0.015em] text-fg">{beat.title}</p>
              <p className="mt-1 text-[13.5px] leading-relaxed text-muted text-pretty">{beat.body}</p>
            </div>
            <div className="mt-3 flex gap-1.5">
              {beats.map((b, i) => (
                <button
                  key={b.title}
                  type="button"
                  aria-label={`${i + 1}: ${b.title}`}
                  aria-current={i === stop ? "step" : undefined}
                  onClick={() => setStop(i)}
                  className="group/stop flex h-4 flex-1 items-center"
                >
                  <span className={`relative block h-0.5 w-full overflow-hidden rounded-full transition-colors ${i < stop ? "bg-fg/45" : "bg-line-2 group-hover/stop:bg-subtle"}`}>
                    {i === stop &&
                      (reduced ? (
                        <span className="absolute inset-0 bg-fg/70" />
                      ) : (
                        <span
                          key={`${at}-${stop}`}
                          onAnimationEnd={next}
                          className="absolute inset-0 origin-left bg-fg/70"
                          style={{
                            animation: `tour ${beat.focus ? DWELL.part : DWELL.whole}s linear forwards`,
                            animationPlayState: running ? "running" : "paused",
                          }}
                        />
                      ))}
                  </span>
                </button>
              ))}
            </div>
          </div>
        </div>
      </Frame>
    </div>
  );
}
