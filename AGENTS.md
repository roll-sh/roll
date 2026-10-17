# roll — agent guide

Monorepo for roll, serverless infrastructure. Use pnpm for the web side (`apps/*`, `packages/*`) and a Cargo workspace for the Rust side (`crates/*`).
`apps/dashboard/AGENTS.md` has extra Next.js rules (Next 16, which differs from what you may know).

## Brand

- The name is **roll**, lowercase. Never write "roll.sh" in a logo, lockup or UI title.
- **Horizon is THE logo.** It is a dome cut into three horizontal bands that sits on a base line.
  - It is `HorizonMark` / `RollLogo` in `@roll/brand`.
  - The same mark is used in the header, sidebar, favicon, avatar and every other place where the logo appears. Don't invent variants.
  - It has to be recognisable at 16px.
- **The dot globe (`RollMark`) is a hero/status visual, not the logo.** Use it big: hero sections, loading screens and status. Never put it in a header.
- Geometry lives in `packages/brand/src/geometry.ts`.
  - Never redraw the mark by hand.
  - For static output, use `horizonSvg`, `avatarSvg` and `markSvg`.
  - After changing geometry, regenerate the assets with `pnpm --filter @roll/brand export`.

## Aesthetics

- The style is minimal and monochrome, Vercel-like but quieter.
  - Dark-first: `#000` background, `#ededed` foreground, with hairline borders (`--line`, `--line-2`).
  - Light mode is a full override, not an afterthought.
- **Accent is a subtle blue, `#4c8dff`**, and is used sparingly:
  - one accent dot on the globe
  - active or busy states
  - the primary text link
  - Never use orange. Never use gradients for decoration.
- Colours come from the CSS variables in `apps/dashboard/src/app/globals.css` (Tailwind classes such as `bg-panel`, `text-muted`, `border-line` and `text-accent`) and from `@roll/brand/tokens` in JS.
  - Don't hard-code hex values in components.
- Type is Geist Sans for UI and Geist Mono for ids, code and numbers.
  - Headlines use tight tracking (`-0.04em` to `-0.05em`) and weight 600.
  - Body text is 13–14px in the dashboard.
- Icons are `lucide-react` at 14–16px with a 1.5 stroke.
- Spacing is generous. Panels are flat (`bg-panel`, 1px border, radius 8–12) with no drop shadows.

## Motion and interactivity

The product should feel alive: dots react to the cursor and the marks "think". Every animation must still be calm and must honour reduced motion.

- **States.** These are shared by every mark: `idle | thinking | deploying | success | error | sleep`.
  - Drive them through `MarkStateProvider` (`useMarkState().play(steps, then)`) so the header logo, hero globe and dashboard all react together.
  - Don't run a separate local animation for these states.
- **Horizon (logo):**
  - Solid at rest.
  - It dissolves into dots when the pointer comes close or when the state is busy.
  - Effects per state:
    - thinking: a horizontal sweep
    - deploying: rows rise
    - success: a ripple
    - error: red plus a small jolt
    - sleep: alpha 0.45
  - Below 56px it uses one row of dots per band plus a 0.2 ghost silhouette, so the shape always reads. Keep hover working at small sizes.
- **Globe (`RollMarkEngine`):**
  - Spring physics.
  - The pointer repels and glows nodes (`reach` controls the radius).
  - Clicks send a burst ripple.
  - It can morph into the horizon formation.
- **DotField** is the background grid. The cursor makes it swell and glow, clicks send ripples, and it sweeps while thinking. Use it behind heroes and "network" cards, never behind dense text.
- **AI "thinking" UI** (`components/ask.tsx`):
  1. The mark switches to `thinking`.
  2. Step lines appear with the `.shimmer` text effect.
  3. The answer streams in with a `.caret`.
  4. The mark plays `success`.
- **Rules for every animation:**
  - Use canvas plus `requestAnimationFrame` for anything with more than a handful of moving parts, and CSS for simple fades (`.rise`).
  - Pause offscreen with IntersectionObserver.
  - Use `useReducedMotion()`. When it is on, render the static final frame, but still reflect state changes.
  - Resolve CSS colours through `resolveColor` (Tailwind v4 emits oklch). Engines cache colours, so call `setOptions` when the theme changes.
  - Ease with springs or ease-out curves. Keep durations at 150–300ms for UI and around 1–2.4s for state loops. Avoid bounce and avoid flashing.
  - Passing `undefined` to engine `setOptions` must not reset an option. Keep that filter.

## Code conventions

- **TypeScript:**
  - React 19 and Next 16 (`cacheComponents`, Turbopack).
  - Run `pnpm typecheck` before you finish; it runs `next typegen` first.
  - Brand components are client components that take `size`, `state`, `interactive` and `accent` props.
- **Rust:**
  - Edition 2024. Declare dependencies in the root `Cargo.toml` `[workspace.dependencies]`.
  - `crates/pg-proxy` is the Postgres wire proxy. `crates/pg-control` is the control plane. `rolld` is the daemon and `roll-cli` is the `roll` CLI.
  - Run `cargo build`, `cargo test` and `cargo clippy` before you finish.
  - After touching anything Postgres-related, run `pnpm test:pg` (Docker e2e; it skips itself when nothing relevant changed). New Postgres features need an e2e test in `crates/pg-control/tests/e2e.rs`.
- **Local state:**
  - `.roll/` holds SQLite and branch data. Never commit it.
  - Test Postgres with `docker run --rm postgres:18-alpine psql "postgres://...@host.docker.internal:55432/main?options=endpoint%3D<ep>"`, because there is no local psql.
