# roll code agents: design system

The visual system of the agents surface: the popup over every dashboard page, `/dashboard/agents`,
and the desktop app's Agents window. Everything else in the dashboard follows AGENTS.md's
"Aesthetics" (quiet, flat, no shadows). The agents surface is the one place that has depth,
because the user asked for it: "better designed than Linear", with T3 Code's look and motion.

The tokens are in `apps/dashboard/src/app/globals.css`. The surface is scoped by `[data-agents]`
(on the popup's dialog and on the full-window page). Inside the scope, the shared tokens (`--bg`,
`--panel`, `--line`, `--muted`…) take the agents values, so a plain `bg-panel` or `border-line`
in an agents component already sits on the right layer. The agents-only tokens are on `:root` too,
because Base UI popups portal to `<body>`.

## Principles

1. **Structure is felt, not seen.** Layers do the separating; hairlines are faint. Spacing
   replaces rules wherever it can. *(Linear 2026 refresh)*
2. **The inverted L.** The chrome (header, threads sidebar) sits flush on the darkest base. The
   working area (the canvas) is one inset, rounded panel a step up. What you open floats as a
   sheet a step up again. *(Linear's app)*
3. **Depth only where things float.** Cards, sheets, popovers and the composer have shadows.
   Chrome and the canvas don't. *(T3 Code, Linear)*
4. **Colour means state; identity tells things apart.** The accent `#4c8dff` is for focus,
   active, running and waiting. Green means done, red means failed. The only other colours are a
   project's or machine's own ("Identity"), and they never take a state's place. Never orange, and
   never a decorative gradient.
5. **Motion is fast and purposeful.** It comes from where the thing comes from, is never
   bouncy, and is static under reduced motion.

## Surfaces (elevation)

| Level | Token | Dark | Light | Used for |
|---|---|---|---|---|
| 0 base | `--bg` | `#08090a` | `#f1f1f2` | header, threads sidebar, around the panels |
| 1 canvas | `--canvas` (`bg-canvas`) | `#0d0e10` | `#fafafa` | the inset canvas panel, under the dot field |
| 2 sheet | `--sheet` (`bg-sheet`) | `#111214` | `#ffffff` | side panel (conversation, settings), dialogs |
| 3 card | `--panel` = `--raised` | `#141518` | `#ffffff` | canvas cards, the composer, path cards |
| 4 fill | `--panel-2` | `#1a1b1e` | `#f2f2f4` | inputs, fills inside cards, icon tiles |
| 4 popover | `--popover` (`glass`) | `#1a1b1e` at 88% plus blur | `#ffffff` | menus, selects, the palette, the card toolbar |
| 5 pressed | `--panel-3` | `#222327` | `#e8e8eb` | pressed and active controls |
| message | `--message` | `#1d1e22` | `#eeeef0` | the user's turn |

Each step is about +2–3 L, with almost no chroma. That matches Linear's measured ladder
(`#08090a`, `#0f1011`, `#141516`, `#191a1b`) and Raycast's greys (`#07080a`, `#0c0d0f`,
`#111214`). Inside `[data-panel]` (the side panel), `--bg` is the sheet colour, so conversation
code that says `bg-bg` matches the sheet it's in.

On glass or sheets, hover fills are translucent foreground: `bg-fg/[0.04]` for hover,
`bg-fg/[0.06–0.07]` for selected or open. That's T3's `--accent` at white/4%.

## Lines and highlights

| Token | Dark | Light | Used for |
|---|---|---|---|
| `--line` | white 5.5% | ink 6.5% | dividers inside cards, quiet field borders |
| `--line-2` | white 8.5% | ink 10% | card, sheet and tab borders (the default) |
| `--line-3` | white 13% | ink 16% | hover, strong |
| `--outline` | white 8.5% | ink 9% | portaled popups (same as `line-2`, but on `:root`) |
| `--highlight` (`inset-shadow-highlight`) | inset 0 1px white 5% | none | top edge of raised things: cards, tabs, sheets, popovers, tiles |

The alphas come from Linear (5% translucent, 8% default, 12–15% strong) and T3 (`--border`
white 8%). The highlight is Raycast's signature `inset 0 1px #ffffff0d`. Don't put the
highlight on flat chrome.

## Shadows

| Token | Dark | Used for |
|---|---|---|
| `shadow-card` | `0 1px 2px /35%, 0 8px 20px -12px /70%` | canvas cards, the primary button |
| `shadow-card-hover` | `0 1px 2px /40%, 0 16px 32px -14px /80%` | a hovered or selected card |
| `shadow-float` | `0 1px 2px /35%, 0 14px 32px -18px /75%` | the composer (T3's `shadow-composer-dark`) |
| `shadow-pop` | `0 4px 12px -2px /35%, 0 18px 44px -18px /80%` | menus, popovers, the card toolbar, the zoom bar |
| `shadow-sheet` | `0 24px 72px -20px /90%` | the side panel, dialogs, the palette (T3's `dialog-glass`) |

Shadows are deep and pulled in under the surface, never halos. Light mode uses the same shapes
at 5–26% ink.

## Canvas engine colours

`canvas-engine.ts` resolves these through `resolveColor`, which drops alpha, so they're opaque:
`--dot` (dark `#7e828b`, drawn at 24% alpha) and `--edge` (`#2e3035`). `--canvas` is used to
punch out the edge ports. `refreshColors()` reruns on a theme change (the canvas listens for
`prefers-color-scheme`).

- **Dot field:** a 28px grid, readable at rest. The pointer swells it, a click ripples it, and
  it sweeps while anything runs, through the open canvas only: never within 56 px of the cards'
  extent (between small cards far out it read as a blob of blue dots behind the work). Keep it:
  the user likes it.
- **Edges** (routed by the layout, `layoutCanvas`): 1.25px, orthogonal, ending exactly at the
  card's side, corners rounded (8 px world, at most half a leg). Left to right, out of the
  parent's right side at its header's middle to a trunk in the gap before its children, then into
  the first card of each grid row (the row's other cards sit beside it, unwired) or a section's
  label line (never its rule). Top-down, a row of lanes (a machine's projects, a project's
  sections) hangs off one bus in the gap above it: out of the parent's bottom 16 px in, along, and
  down into each card's top, the first straight down; a project's grid hangs off a trunk down its
  indent into each row's first card. A thread's request or pinned card is under it in its cell,
  joined by a short drop. A parent's lines are one path stroked once. Ring ports only on cards at
  60 % and closer (smaller they read as ticks). A busy child's last leg (never a shared trunk or
  bus) is the accent at 50% with one travelling dot. No dashes.
- **Busy:** the card's own border breathes between accent at 40% and 18% over 1.6s (still under
  reduced motion). No ring or second outline around the card: a doubled border reads as decoration.

## Cards (`Card` in nodes.tsx)

- **Frame:** `rounded-xl border border-line-2 bg-panel shadow-card inset-shadow-highlight`.
  Clickable cards get `border-line-3` and `shadow-card-hover` on hover. Focus is a 2px accent
  outline offset by 2px.
- **Tones (border):** busy `accent/40`, breathing, done `ok/40` (T3's green for a
  finished run you haven't opened), error `danger/45`, muted at 60% opacity.
- **Selected** (its conversation or settings are open): an accent hairline plus a 3px accent
  halo at 16% (`[data-card][data-selected]` in globals.css).
- **Header:** an `IconTile` (24px, `bg-panel-2`, a hairline, the highlight, a 14px icon), then the
  name in 13.5px at weight 590, then the status. Body text is 12–13px. Meta is 11px, with
  `font-mono tabular-nums` for numbers.
- **Machine cards:** the system line and a sandbox's month are `data-detail`; providers show at
  most three chips and a mono "+N" chip (any needing a login always shows); then the folded
  projects line and, when an agent can't run there, the setup panel.
- **Tools:** `HoverActions` is a floating glass toolbar over the card's top-right corner. It
  appears on hover and on focus; on a touch screen on the card that was tapped (or is open), counter-
  scaled by the canvas's `--zoom` so it stays a finger's size (about 36px). It takes no room in the card, so names never
  truncate.

## The canvas at scale

The rules are in agents.md, "Canvas" › "At scale". How it looks:

- **Fold card** (`FoldedThreads`): a thread card's frame (296 px, the project's full-height left
  edge), "27 more threads" in the title's type, the newest one's time top right, and a summary
  line that peeks (`ChevronRight`). Under it, two sheets of the same card (4 px and 8 px lower,
  inset 8 and 16 px, hairline, no shadow) so it reads as a pile; expanded, no sheets and
  "Show fewer". In the overview its arrow is `CornerDownRight` (it goes into the project); in a
  status group `ChevronDown` (it opens in place).
- **Status strip** (project card): 4 px high, rounded, 1 px gaps, segments in proportion with a
  3 px minimum; information, not decoration, so no gradient and no glow.
- **Section header** (`StatusHeader`): not a card. A 40 px row: an 8 px dot (accent for Needs you
  and Running, which pulses calmly; red Failed; green Done; subtle Quiet), the name at a card
  title's size (14 px, weight 600), the count in mono, over a `--line-2` hairline as wide as the
  section's cards (`--section-w`). No fill, no frame, no edges to its cards. Sections are 80 px
  apart (at least twice a grid's gaps), so each reads as one.
- **Thread cards** are 296 × 128 on the canvas whatever they say (`THREAD_H`), so a grid's rows
  line up. They say what tells them apart, not where they sit (the tree and the identity edge say
  that): provider · model (or "⑂ Fork of …" for a fork on its own card) and one status (the one
  that needs you first; a fork's or subagent's failure is the card's) or when it last did
  something, a title of up to two lines (a hyphenated name never breaks), what it's doing (cut
  after a whole word, `fit-words.ts`), and one row pinned to the bottom: whole chips only (one that
  doesn't fit drops out, never cut) and its usage. In a Done section the header says done: no
  green border or "Completed" on each, just when. Forks and subagents are never cards of their
  own: a chip on the card they came from (the one that matters most, its status dot, "+N"; a click
  opens it), and what waited on them waits on that card. A request stacks under its thread, a
  grid's gap apart. The fold card is a cell of the same size, its project's last: "15 more
  threads", its summary and the newest two titles, two sheets under it.
- **Card tools** float above the card's top-right corner on the popover fill, never over its status.
- **Far out**: never bigger type. Below 55 % (`[data-brief]`) a card's text other than its title
  goes transparent (boxes, chips, dots and frames stay; identity tiles stay whole); below 40 %
  (`[data-greek]`) a card is its frame, its identity (tile, edge, header tint, status strip) and
  its title as one even bar per line, as long as the line (`--fg` at 22 %, 28 % in light mode, a
  band 0.62 em high, cut per line in both engines); icons, dots, chips and buttons go. In light
  mode the frames go to `--fg` 26 % below 55 %, and the done tone is `--ok` at 70 % at any zoom. The identity edge widens to 6 px
  world below 40 % and 10 px below 30 %, so it stays about 2 screen px.
- **Minimap**: 168 × 112, glass like the zoom bar, 12 px above it; blocks are `--muted` for
  machines and projects, the line colour for the rest, accent for requests and busy work; the
  frame is a 1 px `--fg` line at 55 %. Each axis is scaled on its own to fill the box (never more
  than 2.5 × the other), and the accent stays a signal (requests at 55 %, busy work at 35 %). Hidden
  when everything fits, on a phone, and on a canvas under 1200 px (beside a panel).
- **Next waiting**: an accent `CircleDot` and the count in mono in the zoom bar, only while
  something waits.
- **Breadcrumb**: the views bar's chip reads "All projects › site ×"; "All projects" is a button.

## Identity

The user said the canvas was "too black": every project, machine and sandbox now carries a colour
and a face, so you can tell them apart at a glance, calmly. The rules are in agents.md, "Identity";
the components are `ProjectIcon`, `Monogram`, `MachineIcon`, `ColorPicker` and `KindPicker` in
`components/agents/identity.tsx`.

**Palette.** Eight categorical tokens, `--id-<name>` in globals.css (on `:root`, because popups
portal), one hue each at one lightness and a calm chroma, after Linear's restrained project
colours:

| Token | Dark | Light | Hue |
|---|---|---|---|
| `--id-slate` | `oklch(0.74 0.03 255)` | `oklch(0.52 0.03 255)` | neutral |
| `--id-violet` | `oklch(0.74 0.12 296)` | `oklch(0.52 0.15 296)` | 296 |
| `--id-purple` | `oklch(0.74 0.13 322)` | `oklch(0.52 0.16 322)` | 322 |
| `--id-pink` | `oklch(0.75 0.12 350)` | `oklch(0.55 0.16 350)` | 350 |
| `--id-rose` | `oklch(0.73 0.12 12)` | `oklch(0.54 0.15 12)` | 12 |
| `--id-teal` | `oklch(0.76 0.10 180)` | `oklch(0.54 0.09 180)` | 180 |
| `--id-cyan` | `oklch(0.76 0.10 218)` | `oklch(0.54 0.10 218)` | 218 |
| `--id-green` | `oklch(0.76 0.11 148)` | `oklch(0.54 0.11 148)` | 148 |

No orange, amber or yellow (hues 35–115), and nothing within 30° of the accent's 259, so a
project's colour never reads as running. Green and rose are softer than `--ok` and `--danger`.
`agents-identity.test.ts` reads globals.css and fails on any of that.

**Where it shows.** Components set `--tint` (`tint(color)`) and use `text-(--tint)` and
`bg-(--tint)/N`:

| Where | How |
|---|---|
| A project's tile (logo, monogram, emoji) | the logo as found (rounded 25%); else letters in the colour on the colour at 18% (T3's tile) |
| A machine's tile | its kind's lucide icon in the colour, on the colour at 16% |
| Machine and project cards | the 24px tile, and a flat 6% tint behind the header, over a hairline |
| Thread and subagent cards | the project's 14px tile in the meta line, and the card's left edge in the colour: 3px, the whole height, round with the card's corners (over the 1px border, not a bar floating inside it) |
| Edges | the child's colour at 42% (busy edges stay the accent at 50%) |
| Sidebar, ⌘K, launcher, breadcrumb, settings titles, automations list | the same tiles at 14–16px |

**Don't** tint a whole card or its border (the border is its state's), use an identity colour for
state, or draw a project's logo inline (only in an `<img>`).

## Chrome

- **Header (48px):** flush on the base, with no rule under it. It holds the sidebar toggle, the
  Horizon mark (22px, the shared state), "Agents" (14px, weight 590), a divider, the status
  (a dot plus "2 running"), and an accent pill with the waiting count. A small uppercase
  "Demo" tag shows in the demo. On the right: the palette field (a hairline field with a ⌘K
  chip), muted icon buttons, the white primary "New agent" (with a highlight and shadow,
  `active:scale-[0.97]`), a divider, and Close. On a phone the waiting pill shows only its count,
  and Add a machine and Settings move under More.
- **Views bar (44px):** inside the canvas panel. The selected tab is a raised chip
  (`bg-panel-2`, `border-line-2`, highlight, `shadow-card`). Other tabs are bare text with a
  translucent hover. Counts sit in 16px round badges. The filter field is the same quiet field as
  the palette trigger.
- **Display popover:** "Display" beside the filters (the filters' icon is the funnel). Rows of a
  muted label and a `Segmented` (layout, density, grouping), then "Show" with `Switch` rows, a
  hint in each row's tooltip. The compact density hides `[data-detail]` inside cards (globals.css)
  and lays out with `COMPACT_GAPS`.
- **Shelf popover:** "Shelf" (archive icon, a mono count) before Display; absent when nothing is
  shelved. A 400px popover (the phone's width less 8px each side) scaling from the trigger: a
  bare search row over a hairline, a full-width `Segmented` of the three shelves with counts,
  then 32px option rows under 11px project headers (icon, name, count), the active row
  `bg-fg/[0.07]` with its restore button in place of the time, and an 11px key hint footer (not on
  phones). The list's height animates (`AnimatedHeight`) as tabs and searches change it.
  Settled and archived conversations never sit on a card.
- **A machine's folded projects:** one 11.5px subtle line at the foot of the machine card (up to
  three 14px tiles overlapping by 4px, each on a 1px `bg-panel` rim, the count, a chevron),
  `fg/4%` on hover. It opens a 300px popover to the card's right (`openOnHover`, 250 ms in,
  180 ms out): an 11px header, 2-line rows (20px tile, the name at 12.5px, the path or repo in
  10.5px mono), the row's tools (+, pin, gear) fading in on hover or focus (always on touch),
  "Add a project" under a hairline. No empty project cards on the canvas.
- **Keyboard focus on the canvas:** a focused card gets the 2px accent outline offset by 2px,
  like any focus; the focus-a-project chip is a raised chip (`bg-panel-2`, highlight) in the
  views bar with the project's name and an ×.
- **Canvas panel:** `mx-2 mb-2 rounded-xl border border-line bg-canvas`. On a phone it's full
  bleed.
- **Zoom bar:** glass, `rounded-[10px]`, `shadow-pop`, highlight, 28px buttons, the zoom in
  tabular mono. Fit (`fitPlan`) shows everything if that reads (75 % or more, a phone 100 %);
  else, framed from the top, it zooms out as far as 55 % (titles still read) to keep everything
  running, waiting on you or failed in view, clear of the prompt bar and the minimap's corner. The
  layout is planned for that zoom (`planWraps`); growing right, when the machine and project cards
  would starve the grids it lists down instead (each card over what it holds, the grids the
  whole width), and a phone always does. Fit again, when the first couldn't show everything, lays
  the canvas out for everything at once and frames it; when everything shows, it only refits.
- **Side panel:** a floating sheet, `rounded-xl border-line-2 bg-sheet shadow-sheet`, inset 8px
  from the window and level with the canvas panel's top. The resize grip is a 3px pill that
  appears in the gap on hover and turns accent while held. On a phone it covers the canvas.
- **Threads sidebar:** flush on the base with no rule. Rows are 30px; titles are 13px in
  `text-soft`, weight 510 when open. Hover is `fg/4%`; selected is `fg/7%` plus the highlight.
  Meta is 10.5–11px tabular.
- **Popovers and menus:** `glass` with `border-outline`, `shadow-pop` and the highlight. Rows are
  28px at 13px, the highlighted row is `bg-fg/[0.07]`, and icons are muted 14px at stroke 1.75.
  `ui/cn.ts`'s `glass` is the one definition for Base UI popups.
- **Dialogs and the palette:** a `--scrim` (black 60%, 2–3px blur), then a sheet that is
  `rounded-2xl border-line-2 bg-sheet shadow-sheet`, with a 17px/590 title and a 12.5px muted
  lead. `ui/dialog.tsx` (T3's Base UI dialog) is the one to use for new ones: centred a third
  of the way down, it grows from the press or the focused control that opened it (scale 0.98 and
  a fade, 200 ms; none with reduced motion), traps focus and gives it back, and closes on Escape,
  a press outside or its ×. Dialogs, popovers, menus and selects portal to the body, outside
  `[data-agents]`; opened from the surface (`AgentsLayer`, `ui/layer.tsx`) their portal element
  carries `data-agents data-layer`, so they take the agents ladder (`--fg`, `--muted`, `--line`,
  `--panel-2`) in both schemes, and the same components elsewhere (the share page, Connect) keep
  the dashboard's. A new portal in the surface spreads `useLayer()` on its element. Work that takes a while inside one is a `Steps` list
  (`project-actions.tsx`): a 16px ring per step (done: `ok` tick; now: an accent dot, the label
  shimmering, its time in mono at the right; next: subtle; failed: `danger`).
- **"Only roll" chips:** focusing a project puts a raised chip in the views bar ("Only" muted,
  the name, an ×) and the same chip atop the sidebar's list.
- **"Can't run an agent here":** the machine card's one accent-bordered panel (`border-accent/35`, 8px radius; the only accent on the card): a 12px sentence, the primary step (Log in to …, or Add from the ACP registry) and a quiet bordered "Use an endpoint or gateway", then an 11px subtle line with the rest as underline-on-hover links. The same panel in the launcher; a project card says it in one 11.5px subtle line with "Set one up".
- **Add a project:** the 520px dialog: "On" and a machine `Select` (several online), a
  full-width `Segmented` (Folder, Clone a repo, New repo), lists in a 192px inset well
  (`bg-bg`, hairline, 8px radius; rows 28–32px, a project's own folder or repo marked "project"
  / "On ada-mbp" in subtle 11px), the primary action under it, then Pull merged changes under a
  hairline. Working: the `Steps` list in place of the list. Done: the project as a raised row
  (32px tile, name at 14px/590, machine and path in mono, an `ok` tick), then Start an agent
  (primary, focused), Open settings, and a quiet "Add another" at the right.
- **Empty and loading states:** loading shows the Horizon mark (40px, thinking) over a
  shimmering line. With no machines, the globe (`RollMark`) shows big over the `DotField` with
  both ways in. An empty view is one calm sentence with a text link back.

## Type

Geist Sans and Geist Mono, using variable weights the way Linear does: 400 body, 510 labels and
tabs, 590 titles.

| Role | Size and weight | Tracking |
|---|---|---|
| display (empty state) | 28–32 / 590 | −0.05em |
| dialog title | 17 / 590 | −0.025em |
| surface title ("Agents") | 14 / 590 | −0.02em |
| card title | 13.5 / 590 (machine, project), 13.5 / 500 (thread) | −0.015em |
| body | 13 / 400 | −0.005em |
| label, tab | 12.5 / 510 | −0.01em |
| meta | 11–11.5 / 400 | 0 |
| micro and counts | 10–10.5 mono, tabular | 0 |

Text levels: `--fg` `#f7f8f8`, `--soft` `#d0d6e0` (secondary body), `--muted` `#8a8f98`, `--subtle`
`#62666d`, `--faint` `#45474d`. These are Linear's four, plus a faint step for disabled text.

## Spacing, radii, icons

- **Spacing:** a 4px grid. Card padding is 12px; chrome gutters are 8px; gaps are 4, 8 and 10px.
- **Radii:** 6 for controls (`rounded-md`), 8 for tiles and toasts, 10 for popovers and the zoom
  bar, 12 for cards and sheets (`rounded-xl`), 16 for dialogs and the palette (`rounded-2xl`),
  full for pills.
- **Icons:** lucide at 14–16px with stroke 1.75 in chrome (T3) and 1.5 in dense lists.

## Motion

| What | How |
|---|---|
| popup opening and closing | `PopupFrame` (agents-control.tsx, presence): translateY 10px, scale 0.985 → 1, fade, 260ms on `--ease-drawer`; out the same way over 170ms (`POP_OUT`) however it closes, inert, then unmounts |
| chrome settling | `.chrome-in`: translateY −4px, fade, 300ms on the drawer curve, 50ms after the sheet |
| the right sheet (conversation, settings, automations) | one frame (`RightDock`, right-dock.tsx): an in-flow gap grows to the sheet's width with the sheet pinned to its left edge, so the sheet slides in from the window's edge and the canvas narrows in step; 260ms on the drawer curve, the sheet fading in over 200ms. Closing is the same backwards over 260ms on `EASE_LEAVE` (the standard ease-in-out: on an ease-out a closing sheet covered most of the way in 100 ms and read as gone at once), holding its opacity for 100ms before fading, inert meanwhile, then it unmounts |
| switching what the right sheet shows | the frame stays put; the new pane fades in (and 8px across) over the old one fading out, 200ms on `--ease-out-soft`. Never a second sheet, never an empty frame |
| the maximized builder | the sheet's gap grows over the working area while the canvas folds (margins, border, opacity) beside it; the canvas stays mounted |
| threads sidebar (⌘B) | `EdgeSheet` (edge-sheet.tsx): the same gap from the left edge, 268px, both ways; not on the frame it opens because it was left open |
| phone sheets | the conversation, settings and automations slide over the canvas from the right edge, the sidebar from the left; back out the same way |
| a page in the automations sheet, a view in the ⌘K palette | `SlidePages` (slide-pages.tsx): deeper comes 16px in from the right, back from the left, fading in over 200ms on `--ease-out-soft`, while the page left behind slides the other way and fades over 150ms, out of the flow and inert; a quick back-and-forth turns the same page around; a tab on the same level doesn't move. The palette's height eases between views (`useHeightGlide`, 200ms) |
| popovers and menus | scale 0.98 → 1 plus fade, 150ms, `--ease-out-soft` `(0.23,1,0.32,1)`, from the trigger; out over 120ms, inert. Hand-built ones (a field's suggestions, the composer's commands, the switcher, the filter editor) the same way through `Pop`; the filter editor stays put across tabs |
| palette and dialogs | scale 0.98 plus fade, 200ms on the drawer curve, growing from the press or control that opened it, with the scrim fading in over 200ms; out the same way over 170ms (`POP_OUT`), inert, however it closes. The launcher (new agent) is one too, in the surface; asked for elsewhere while open, it glides there |
| what a dialog shows changing (add a project's form → Project added, a sandbox's form → its steps, the gateway wizard's steps, the add-machine sandbox's loading → form → setting up) | the sheet stays; its height eases to fit (`useContentGlide`, 200ms on the emphasized ease-out, from wherever it is, the scroller's bar hidden meanwhile) whenever its header, body or footer changes size; the content slides as `SlidePages` (a step on from the right, Back from the left, a change of state in place), and a title or lead that changes crossfades inside one `Title` (the dialog keeps its name). A part that only appears (a step's result, an error) fades in (`.fade-in`). A leaving page keeps its last render, so a form that held a secret empties it in a commit of its own (`flushSync`) before it gives way |
| toasts and notices | in from the right edge (automation notices, 300ms ease-out quint) or rising 6px (the undo notice, a drop on the canvas that couldn't launch), out the way they came over 170ms, inert and no longer announced; the toasts left glide into place |
| disclosures (`Collapse`) | the height eases open and shut (200ms); shutting folds what it showed, then unmounts it; it clips from the first frame. A plan's "saved to workspace" line folds open and, 2.4s later, away (it vanished) |
| clamped text opening (a long message's Show full message, a plan's Expand plan) | the text's height eases between the clamp and the whole (`useHeightGlide`, 200ms on the emphasized ease-out), clipped meanwhile |
| a lone step in the work log that becomes a group | the step's row crossfades into the group's row in place (`SlidePages`, 200ms) |
| tabs and segmented controls | the selected look is one thumb under the choices (`SlidingThumb`, ui/thumb.tsx) that slides and resizes to the new choice, 200ms on `--ease-out-soft`, interruptible; the choices only change colour (`Segmented` everywhere, the canvas's view tabs) |
| list rows (the queue, the threads sidebar, the shelf) | `useFold`/`useFlip` (list-motion.ts): a row that arrives folds open (height and fade, 200ms), one that leaves folds shut (160ms) inert before it unmounts, and rows that change places slide (200ms); a row settled, snoozed or woken slides to its shelf, or folds out when the shelf is shut. Only a change of order slides rows. A tab or a search on the shelf shows its rows at once. A project in the sidebar folds open and shut (`Collapse`) |
| hover and colour | 150ms; buttons press to scale 0.97 |
| cards | ease to their layout slot (spring-like, 0.22 per frame); new cards grow from their parent and rise in; a card leaving the graph (filtered out, hidden by Display, gone) fades and shrinks to 0.96 where it was over 160ms (`CARD_OUT`), inert and out of the pointer's and the arrows' way, then unmounts; the engine keeps drawing its edges where it was, fading over the same 160ms ease-in, then drops them (they vanished at once) |
| state loops | the busy border and pulse dots breathe over 1.6s; shimmer runs 1.8s; no flashing |
| a pill that opens into a composer (the canvas's prompt bar) | one structure in both states: the text line never moves; the rest grows from nothing (grid rows 0fr → 1fr) while the radius eases from a pill to 16px, 240ms on the emphasized ease-out `(0.2,0,0,1)` (seen moving the whole way; on `--ease-out-soft` it was 78% there in 60ms and read as a jump); its contents fade and rise 4px, 60ms behind; closing is the same backwards |

The drawer curve `cubic-bezier(0.32,0.72,0,1)` is T3's `--ease-drawer`. Every keyframe class is
switched off under `prefers-reduced-motion`, and the engine draws a static, state-correct frame.

**How (the rules for anything that opens, closes or switches):**

- **Exits are real.** Something that goes away animates out, then unmounts. Never
  `{open && <X/>}` for a surface that opened with motion. React-owned surfaces go under motion's
  `AnimatePresence` and drive their values with `usePresenceTracks` (`presence.ts`): every change
  animates from where the value is, so open → close → open turns around without a jump (motion's
  declarative `exit` replays `initial` when a child comes back, which jumped a reopened sheet to
  nothing). Base UI popups and dialogs keep their Root mounted and animate with
  `data-starting-style`/`data-ending-style`. A dialog shown for a value goes through `useKept`
  (`ui/dialog.tsx`): mount it while the kept value is set, with `open` and `onExited`, so it stays
  until its exit ends; a dialog opened from a row that can leave its list (a row's snooze) is
  hosted outside the list. Every modal is the shared dialog (`Dialog`, or `DialogSheet` for its own
  layout, as the launcher has): never a hand-built `role="dialog"`. Base UI only plays the way in
  when `open` turns on in a mounted Root, so `DialogSheet` commits its Root closed once and opens
  it before paint. CSS keyframe classes are for entrances only: a keyframe
  can't be interrupted.
- **What's leaving never blocks input.** It's `inert` from the moment it starts to leave, so a click
  lands on what's behind it at once; focus left inside it counts as lost.
- **Layout moves with it.** A panel that takes room takes it through an in-flow gap animating its
  width (T3's sidebar gap), so what's beside it moves in step instead of snapping at the end.
- **Switches crossfade in place.** The container stays; the content changes inside it.
- **Reduced motion** sets every duration to 0: the end state at once, every state change still shown. An entrance starts at its shown value (`useEnteringValue`), and Base UI starting styles are `motion-safe:` only: a duration of 0 still drew one frame at opacity 0 (a blank frame between two pages, a dialog a frame late).
- **Timing lives in `lib/agents-motion.ts`** (`DRAWER`, `LEAVE`, `FADE_IN`, `PANE_SWITCH`).
- **Libraries:** motion (motion.dev) for presence and in-place switches; Base UI's transitions for
  its popups; CSS for toggles of things that stay mounted. Not the View Transitions API: it takes
  the pointer until it ends, freezes the live canvas in a snapshot, and a second transition skips
  the first to its end.

## List, board and the bulk bar

The list and the board (`thread-list.tsx`, `thread-board.tsx`) are the same surface as the canvas,
in its inset panel. They follow Linear's density, not the canvas's cards:

- **List rows** are 34 px, one line, 13 px text, the title in `text-soft` with what it's doing after
  it in `text-subtle`; ids, times, counts and cost in Geist Mono. Group headers are 34 px on a faint
  `bg-panel/40` with a hairline. A row's left edge is its project's colour (2 px, the full height,
  as on its card), the keyboard cursor's edge is accent. The open conversation's row is
  `bg-fg/[0.06]`; a selected row `bg-accent/[0.07]`. The checkbox shows on hover and always once
  anything is selected. Headers are sticky over a translucent `bg-canvas`.
- **Board cards** are the canvas card's frame made small (`bg-panel`, `border-line-2`, radius 10,
  `shadow-card`, the project's colour as a full-height left edge), 92 px high: the status pill and
  time, two lines of title, then project · machine, PR, +/−, cost. Columns are 284 px; an empty
  status column is 152 px so it stays without pushing the others away. The only drop target (Done)
  takes an accent wash and a hairline while something is over it.
- **The bulk bar** is part of the frame: a 40 px strip under the views bar, opaque (`--accent` at
  5 % over `--canvas`), a hairline under it, folding open and shut with `Collapse`. Actions are
  ghost buttons with lucide icons; disabled ones fade to 35 %. No floating bar, no glow, no
  second outline. Selected canvas cards get an accent hairline and a 7 % accent fill; their
  coloured left edge stays.
- **Motion:** rows and cards move by `transform` (180–200 ms ease-out) when a sort or a state
  change reorders them; a card that changes column slides over from its old place (260 ms); the
  layout switcher's thumb slides. Reduced motion: they jump.

## Do / don't

- **Do** use the tokens (`bg-canvas`, `bg-sheet`, `bg-panel`, `border-line-2`, `shadow-pop`,
  `inset-shadow-highlight`, `glass`). **Don't** hard-code a hex, an rgb or a shadow in a
  component.
- **Do** put the accent only on focus, active, running and waiting, and a project's or machine's
  colour only on its tile, header tint, a thread card's left edge and the canvas edges. **Don't**
  float a short bar inside a card, stop a coloured edge short of the corners, or put a second
  outline around a card. **Don't** tint whole cards, and **don't**
  use orange or decorative gradients.
- **Do** float tools over cards. **Don't** reserve invisible room for hover buttons.
- **Do** let layers separate things. **Don't** add a border-r or border-b where a panel edge
  already does it.
- **Do** use the Horizon mark in the header (`HorizonMark`). **Don't** put the globe in a bar.
- **Do** honour reduced motion. **Don't** animate anything that isn't changing state.
- **Do** close something on a press outside it and its popups, on Escape, or when focus moves to
  a real element elsewhere. **Don't** close it on a blur alone: Safari and Firefox on macOS don't
  focus buttons on click, so `relatedTarget` is null while a popup's own buttons are used. Never
  decide from `relatedTarget` alone. A hand-built popup uses `useDismiss` (dismiss.ts) for this, and
  `Pop` (pop.tsx) to come and go: never `{open && <ul className="pop">}`, which leaves in a frame.

## Pickers

Nobody types an id, a key or a handle that roll can look up (the user's ask, wave 9). Every field
that names a thing — a Linear team, state or person, a Slack channel, a GitHub label, a roll
thread or machine — is the shared picker, `Picker` in `components/agents/ui/picker.tsx`
(`LookupPicker` in `lookup-picker.tsx` for an integration's things):

- **The trigger is the field.** A hairline box like every field showing the picked thing by name
  with its picture, colour or kind icon; a list is chips (each with its ×) and an "Add" at the end.
  A value nobody listed yet is looked up by id; one that resolves to nothing shows as itself, in
  mono. A variable shows as its `{{ path }}` in the accent.
- **The list is T3's combobox** on a solid popover (the popover fill, hairline and pop shadow, not glass: rows of names must read over a busy form): search at the top (the platform filters it),
  recents first (per device), then the field's own rows (`roll`, `No one`), then the platform's,
  grouped (a state's team). ↑↓ and Enter, Esc closes; picking several keeps it open, with a
  "N picked · Done" footer.
- **Fallbacks are rows, not modes:** "Use “…”" for a typed value, "Create label “…”" where the
  platform lets roll make one, "Use a variable" in automation steps, "Load more" for the next page.
- **Problems are a quiet footer** (a warning icon, the reason, "Connect" or "Reconnect" linking to
  the owner's settings). The field still takes a typed value.
- The selected row is a quiet fill and a check (a box for lists); no accent bars or rings.
- roll code's pages use the same picker (review requests, required checks, work-item labels),
  with their rows from the code API.

## Automations

Automations live in one right sheet beside the canvas, the conversation's frame (`SidePanel`: the
same width, resize handle and slide), so the canvas stays live and interactive next to them. Inside
it is a stack (list → overview → builder or runs → run → conversation): each page has the same
56px header (Back with where it goes, the page's tile, title and subtitle, its tools, then Maximize
on the builder and Close), and pages push and pop sideways. A conversation opened from a run is a
page of the sheet, never a second sheet. The builder can fill the working area (T3's "Maximize
panel"). The rest (the builder's flow of cards, template fields, the run timeline, notice toasts,
the canvas chip) is in docs/code/automations-ui.md, with the Linear, Zapier, n8n, GitHub Actions,
Vercel, Raycast and T3 references each one comes from.

## References

Researched on 2026-11-04 from public pages only. The screenshots are in
`~/.local/share/roll-agents/shots/design/refs/`. **M** means measured from CSS or source; **E**
means eyeballed.

- **Linear.** Sources: "How we redesigned the Linear UI" (part II), "Behind the latest design
  refresh" (2026), and the StyleX post.
  - Themes are generated in LCH from base, accent and contrast, with 100+ derived tokens
    replacing 98 hand-written ones.
  - Elevation runs background, foreground, panels, dialogs, modals. A selected or focused
    surface re-derives the theme from its own background.
  - The sidebar is "a few notches dimmer". There are fewer separators: "structure should be
    felt, not seen".
  - Marketing CSS (M): levels `#08090a`, `#0f1011`, `#141516`, `#191a1b`. Borders `#ffffff0d`,
    `#ffffff14`, `#ffffff1f`, `#ffffff26`. Text `#f7f8f8`, `#d0d6e0`, `#8a8f98`, `#62666d`.
    Weights 400, 510, 590, 680. Text 13px at −0.01em, titles at −0.012 to −0.022em. Radii 4–32.
    Motion 180ms with ease-out quint.
  - App (E): the inverted L (sidebar flush, the main view an inset rounded panel); the agent chat
    a floating card one step brighter; the composer a raised well.
  - **Taken here:** the ladder, the alphas, the text levels, the weights, the inverted L, the
    floating sheet, and "dim the chrome".
- **T3 Code.** Read from source (M: `apps/web/src/index.css`, `components/ui/*`; and
  `~/.local/share/roll-agents/t3-chat-spec.md`).
  - Dark background `#000`; card and popover are neutral-950 mixed 97% with white; message 92%.
  - Border white 8%, input 11%, hover white 4%.
  - Shadows: composer `0 14px 32px -18px /75%`; popovers `0 18px 44px -18px /80%`; `dialog-glass`
    is `inset 0 1px white 4%, 0 24px 72px -20px /90%`.
  - `--ease-drawer (0.32,0.72,0,1)`; popovers scale 0.98 at about 150ms; buttons press to 0.97.
  - Lucide icons at stroke 1.75; radius 8.
  - **Taken here:** `shadow-float`, `shadow-pop`, `shadow-sheet`, the glass, the drawer curve,
    the popup motion, the green "done" tone, and the translucent hover fills.
- **Raycast** (M).
  - Greys `#07080a`, `#0c0d0f`, `#111214`, `#1b1c1e`; border `#242728`.
  - The signature top highlight `inset 0 1px #ffffff1a` (and `0d` on bars); glass blurs at
    10–36px.
  - **Taken here:** the top highlight on raised things and the near-black greys.
- **Vercel** (M).
  - `background-100` `hsl(0 0% 4%)` over `#000`; gray-alpha `#ffffff0f`, `17`, `21`.
  - Borders drawn as `0 0 0 1px` shadows; the focus ring is `0 0 0 2px bg, 0 0 0 4px blue`.
  - Popover 200ms, overlay 300ms. Its `swift` curve overshoots, which we avoid.
  - **Taken here:** the 2px offset focus ring.
- **Cursor** (E).
  - Near-black with groups under small labels with counts.
  - Two-line rows (title, then muted status); the selected row is a raised fill with no border.
  - **Taken here:** the sidebar's rows and the count badges.
- Figma (light only), Arc (now a farewell page) and t3.chat (behind a browser check) gave
  nothing usable.
