import { horizonBands, resolveColor, useReducedMotion, type MarkState } from "@roll/brand";
import { useEffect, useRef } from "react";
import { inksOf, onSchemeChange, type Rgb } from "./in-view";

/** The dither's cell, in CSS pixels: one dot, on or off. */
const CELL = 2;
/** The 4×4 Bayer matrix: a cell is on when the picture there is brighter than its threshold. */
const BAYER = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5].map((v) => (v + 0.5) / 16);
/** Seconds Horizon takes to rise the first time the scene is seen. */
const RISE = 3.2;

const SKY = 0;
const FAR = 1;
const NEAR = 2;
const GROUND = 3;

/**
 * The hills in front of the mountain, far to near: where each crest sits below the skyline and
 * how much it wanders (fractions of the scene's lower part), how fast it wanders, and its tone
 * (its body, the light along its crest, how far down that light reaches).
 */
const HILLS = [
  { at: 0.06, amp: 0.3, freq: 9, body: 0.05, lit: 0.22, fade: 0.1 },
  { at: 0.27, amp: 0.26, freq: 5.5, body: 0.07, lit: 0.27, fade: 0.12 },
  { at: 0.49, amp: 0.2, freq: 3.4, body: 0.09, lit: 0.33, fade: 0.13 },
];

/** Smooth 1-D value noise, a few octaves of it. */
function ridge(x: number, seed: number) {
  let v = 0;
  let amp = 0.5;
  let f = 1;
  for (let o = 0; o < 4; o++) {
    const p = x * f + seed * 17.3;
    const i = Math.floor(p);
    const t = p - i;
    const a = Math.sin(i * 127.1 + seed * 311.7) * 43758.5453;
    const b = Math.sin((i + 1) * 127.1 + seed * 311.7) * 43758.5453;
    const s = t * t * (3 - 2 * t);
    v += ((a - Math.floor(a)) * (1 - s) + (b - Math.floor(b)) * s) * amp;
    amp *= 0.5;
    f *= 2.1;
  }
  return v;
}

/**
 * The closing's landscape, as an ordered dither: Horizon's three bands rising behind a mountain,
 * a nearer ridge, and ground whose lines roll towards you. The scene is a picture of brightness
 * (0 to 1) worked out per cell; each cell's dot is on when that's over its Bayer threshold, which
 * is all the halftone is. The ridges are worked out once per size; the sun, the haze, the ground
 * and the light are added per frame, at 30 a second.
 *
 * It follows the motion rules: the cursor carries a light that brings out the dots around it
 * (accent where it's brightest), a click sends a ring, the shared mark state plays (thinking
 * sweeps, deploying lifts the sun, success rings out from it, sleep dims), it's paused
 * offscreen, and under reduced motion it's the finished still. Fills its positioned parent.
 */
export function DitherScape({ state = "idle", className }: { state?: MarkState; className?: string }) {
  const ref = useRef<HTMLCanvasElement>(null);
  const stateRef = useRef(state);
  const reduced = useReducedMotion();
  const ringsRef = useRef<{ x: number; y: number; t0: number }[]>([]);
  const sunRef = useRef({ x: 0, y: 0 });
  const still = useRef<() => void>(undefined);

  useEffect(() => {
    stateRef.current = state;
    if (state === "success") ringsRef.current.push({ ...sunRef.current, t0: performance.now() / 1000 });
    // Under reduced motion nothing is running: draw the state's still.
    still.current?.();
  }, [state]);

  useEffect(() => {
    const canvas = ref.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const lo = document.createElement("canvas");
    const lctx = lo.getContext("2d");
    const dots = document.createElement("canvas");
    const dctx = dots.getContext("2d");
    if (!lctx || !dctx) return;

    let w = 0;
    let h = 0;
    let cols = 0;
    let rows = 0;
    let px = 3;
    let cell = CELL;
    let mask: CanvasPattern | null = null;
    let image: ImageData | null = null;
    let base = new Float32Array(0);
    let kind = new Uint8Array(0);
    let hazeX = new Float32Array(0);
    let rowAdd = new Float32Array(0);
    let hy = 0;
    let groundY = 0;
    let R = 0;
    // The land is drawn in the muted ink, Horizon in the foreground's, so the mark stands out of it.
    const read = () => ({ ...inksOf(canvas), sun: resolveColor(getComputedStyle(canvas).getPropertyValue("--fg").trim() || "currentColor") as Rgb });
    let inks = read();
    let raf = 0;
    let running = false;
    let frame = 0;
    let seenAt = -1;
    let lift = 0;
    let busy = 0;
    let dim = 1;
    const pointer = { x: -9999, y: -9999, s: 0, on: false };
    const rings = ringsRef.current;
    const bands = horizonBands().slice(0, 3);

    const build = () => {
      const rect = (canvas.parentElement ?? canvas).getBoundingClientRect();
      w = rect.width;
      h = rect.height;
      if (!w || !h) return;
      // A cell is a whole number of device pixels, so the dots stay crisp at any zoom.
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      px = Math.max(1, Math.round(CELL * dpr));
      cell = px / dpr;
      cols = Math.ceil(w / cell);
      rows = Math.ceil(h / cell);
      canvas.width = cols * px;
      canvas.height = rows * px;
      canvas.style.width = `${cols * cell}px`;
      canvas.style.height = `${rows * cell}px`;
      lo.width = cols;
      lo.height = rows;
      image = lctx.createImageData(cols, rows);
      // One round dot per cell: the picture is cut to these, so on-cells never merge into a slab.
      dots.width = px;
      dots.height = px;
      dctx.clearRect(0, 0, px, px);
      dctx.beginPath();
      dctx.arc(px / 2, px / 2, px * 0.43, 0, Math.PI * 2);
      dctx.fill();
      mask = ctx.createPattern(dots, "repeat");

      base = new Float32Array(cols * rows);
      kind = new Uint8Array(cols * rows);
      hazeX = new Float32Array(cols);
      rowAdd = new Float32Array(rows);
      const below = Math.min(300, Math.max(180, h * 0.32));
      hy = h - below;
      groundY = hy + below * 0.7;
      R = Math.min(w * 0.36, 250, hy * 0.6);
      sunRef.current = { x: w / 2, y: hy - R * 0.5 };
      const mid = w / 2;
      const bell = (x: number, at: number, wide: number) => Math.exp(-(((x - mid - at * R) / (wide * R)) ** 2));
      const crest = new Float32Array(HILLS.length + 1);
      for (let cx = 0; cx < cols; cx++) {
        const x = (cx + 0.5) * cell;
        const u = (x - mid) / 1000;
        // The mountain: a peak left of centre that hides the sun's lower left, a shoulder on its
        // right, lower country either side. Then the hills in front of it, nearer and broader.
        crest[0] = hy - R * (0.1 + 0.56 * bell(x, -0.42, 0.5) + 0.2 * bell(x, 0.7, 0.34) + 0.16 * bell(x, -1.7, 0.7) + 0.1 * bell(x, 2, 0.8)) - (ridge(u * 30, 1) - 0.5) * R * 0.13;
        HILLS.forEach((l, k) => (crest[k + 1] = hy + below * l.at - (ridge(u * l.freq, k + 2) - 0.5) * below * l.amp));
        const behind = Math.exp(-(((x - mid) / (R * 1.5)) ** 2));
        for (let cy = 0; cy < rows; cy++) {
          const y = (cy + 0.5) * cell;
          const i = cy * cols + cx;
          const grain = (ridge(u * 90 + cy * 0.37, 9 + (cy % 7)) - 0.5) * 0.06;
          if (y >= groundY) {
            kind[i] = GROUND;
            base[i] = 0.12 + 0.26 * ((y - groundY) / (h - groundY)) ** 1.3;
            continue;
          }
          let front = -1;
          for (let k = HILLS.length; k >= 0; k--) {
            if (y >= crest[k]) {
              front = k;
              break;
            }
          }
          if (front < 0) {
            kind[i] = SKY;
            // A halo around where the sun rests, and a little haze over the skyline.
            const d = Math.hypot(x - mid, y - (hy - R * 0.5)) / (R * 0.95);
            base[i] = 0.062 * Math.exp(-d * d) + 0.04 * Math.exp(-(crest[0] - y) / (R * 0.12));
          } else if (front === 0) {
            kind[i] = FAR;
            // Lit along its crest, most where the sun stands behind it.
            base[i] = 0.07 + (0.16 + 0.22 * behind) * Math.exp(-(y - crest[0]) / (R * 0.3)) + grain;
          } else {
            const l = HILLS[front - 1];
            kind[i] = NEAR;
            base[i] = l.body + l.lit * Math.exp(-(y - crest[front]) / (below * l.fade)) + grain;
          }
        }
      }
      if (!running) draw(performance.now());
    };

    const draw = (ms: number) => {
      if (running) raf = requestAnimationFrame(draw);
      if (running && frame++ % 2) return;
      if (!image) return;
      const t = ms / 1000;
      const st = stateRef.current;
      // Eased towards the state while running; set outright for a still.
      const ease = (k: number) => (running ? k : 1);
      pointer.s += ((pointer.on ? 1 : 0) - pointer.s) * ease(0.14);
      busy += ((st === "thinking" ? 1 : 0) - busy) * ease(0.1);
      lift += ((st === "deploying" ? 1 : 0) - lift) * ease(0.06);
      dim += ((st === "sleep" ? 0.45 : 1) - dim) * ease(0.1);
      for (let i = rings.length - 1; i >= 0; i--) if (t - rings[i].t0 > 3.5) rings.splice(i, 1);

      // Horizon comes up from behind the ridge once, then rests; deploying lifts it a little more.
      const since = reduced ? RISE : seenAt < 0 ? 0 : Math.min(RISE, t - seenAt);
      const up = 1 - (1 - since / RISE) ** 3;
      const sunBase = hy - R * 0.04 + (1 - up) * R * 0.9 - lift * R * 0.06;
      const glow = 0.82 + 0.18 * up + lift * 0.12;

      for (let cx = 0; cx < cols; cx++) hazeX[cx] = Math.sin(cx * 0.014 + t * 0.19) * 0.6 + Math.sin(cx * 0.0055 - t * 0.11);
      for (let cy = 0; cy < rows; cy++) {
        const y = (cy + 0.5) * cell;
        if (y < groundY) {
          rowAdd[cy] = Math.sin(cy * 0.06 + t * 0.07) * 0.022;
        } else {
          // Lines across the ground, closer together towards the horizon, rolling at you.
          const depth = (h - groundY * 0.96) / (y - groundY * 0.96);
          const line = Math.sin(depth * 5 + t * 0.8);
          rowAdd[cy] = line > 0.35 ? 0.2 : -0.05;
        }
      }
      const sweepX = busy > 0.01 ? ((t * 0.28) % 1.4) * w - w * 0.2 : -1;

      const data = image.data;
      const [lr, lg, lb] = inks.ink;
      const [sr, sg, sb] = inks.sun;
      const [ar, ag, ab] = inks.accent;
      const pr = 150;
      const lit = pointer.s > 0.01;
      const sx = w / 2;
      for (let cy = 0; cy < rows; cy++) {
        const y = (cy + 0.5) * cell;
        const ly = 64 + ((y - sunBase) / R) * 44;
        let band = -1;
        for (let b = 0; b < bands.length; b++) if (ly >= bands[b].y1 && ly < bands[b].y2) band = b;
        const half = band < 0 ? 0 : (bands[band].halfWidth(ly) / 44) * R;
        const dy = y - pointer.y;
        const nearPointer = lit && Math.abs(dy) < pr * 2.2;
        for (let cx = 0; cx < cols; cx++) {
          const i = cy * cols + cx;
          const x = (cx + 0.5) * cell;
          const k = kind[i];
          let v = base[i];
          let sun = false;
          if (k === SKY) {
            sun = band >= 0 && Math.abs(x - sx) <= half;
            if (sun) v = (0.8 - band * 0.11 - ((ly - bands[band].y1) / 12) * 0.07) * glow;
          } else if (k === GROUND) v += rowAdd[cy];
          else v += hazeX[cx] * rowAdd[cy];
          let hot = 0;
          if (nearPointer) {
            const dx = x - pointer.x;
            if (Math.abs(dx) < pr * 2.2) {
              hot = Math.exp(-(dx * dx + dy * dy) / (pr * pr)) * pointer.s;
              v += hot * 0.34;
            }
          }
          for (let r = 0; r < rings.length; r++) {
            const age = t - rings[r].t0;
            const d = Math.hypot(x - rings[r].x, y - rings[r].y) - age * 300;
            if (d > -60 && d < 60) {
              const wave = Math.exp(-(d * d) / 900) * Math.exp(-age * 0.9);
              v += wave * 0.26;
              hot = Math.max(hot, wave * 0.55);
            }
          }
          if (sweepX >= 0) {
            const d = (x - sweepX) / 90;
            if (d > -3 && d < 3) v += Math.exp(-d * d) * 0.22 * busy;
          }
          const o = i * 4;
          if (v * dim > BAYER[(cy & 3) * 4 + (cx & 3)]) {
            const a = hot > 0.25 ? Math.min(1, (hot - 0.25) * 2) : 0;
            const ir = sun ? sr : lr;
            const ig = sun ? sg : lg;
            const ib = sun ? sb : lb;
            data[o] = ir + (ar - ir) * a;
            data[o + 1] = ig + (ag - ig) * a;
            data[o + 2] = ib + (ab - ib) * a;
            data[o + 3] = 255;
          } else data[o + 3] = 0;
        }
      }
      lctx.putImageData(image, 0, 0);
      ctx.globalCompositeOperation = "copy";
      ctx.imageSmoothingEnabled = false;
      ctx.drawImage(lo, 0, 0, cols * px, rows * px);
      ctx.globalCompositeOperation = "destination-in";
      if (mask) {
        ctx.fillStyle = mask;
        ctx.fillRect(0, 0, cols * px, rows * px);
      }
    };
    still.current = () => {
      if (!running) draw(performance.now());
    };

    const start = () => {
      if (seenAt < 0) seenAt = performance.now() / 1000;
      if (running || reduced) return;
      running = true;
      raf = requestAnimationFrame(draw);
    };
    const stop = () => {
      running = false;
      cancelAnimationFrame(raf);
    };

    const onMove = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      pointer.x = e.clientX - rect.left;
      pointer.y = e.clientY - rect.top;
      pointer.on = pointer.x >= 0 && pointer.y >= 0 && pointer.x <= rect.width && pointer.y <= rect.height;
    };
    const onLeave = () => (pointer.on = false);
    const onDown = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      const x = e.clientX - rect.left;
      const y = e.clientY - rect.top;
      if (x >= 0 && y >= 0 && x <= rect.width && y <= rect.height) rings.push({ x, y, t0: performance.now() / 1000 });
    };

    const ro = new ResizeObserver(build);
    ro.observe(canvas.parentElement ?? canvas);
    const io = new IntersectionObserver(([e]) => (e.isIntersecting ? start() : stop()), { threshold: 0.15 });
    io.observe(canvas);
    const offScheme = onSchemeChange(() => {
      inks = read();
      if (!running) draw(performance.now());
    });
    build();
    if (!reduced) {
      window.addEventListener("pointermove", onMove, { passive: true });
      window.addEventListener("pointerdown", onDown, { passive: true });
      document.documentElement.addEventListener("pointerleave", onLeave);
    }
    return () => {
      stop();
      still.current = undefined;
      ro.disconnect();
      io.disconnect();
      offScheme();
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerdown", onDown);
      document.documentElement.removeEventListener("pointerleave", onLeave);
    };
  }, [reduced]);

  return <canvas ref={ref} aria-hidden className={`pointer-events-none absolute top-0 left-0 size-full text-muted ${className ?? ""}`} />;
}
