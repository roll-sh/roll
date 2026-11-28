# The marketing pages

`/`, `/products` and `/products/<slug>` sell roll to someone who has never seen it. They follow
AGENTS.md's look (monochrome, hairlines, the accent used sparingly, Horizon as the logo). Two kinds
of picture explain the products, and they're never mixed up:

- **The dashboard is the dashboard.** Any picture of the product is a render of the real app.
- **A concept is a drawing or a demo.** What a product does (a branch forking, a queue fanning out) is
  drawn in SVG or played on a canvas. Neither ever imitates a dashboard page.

Everything a page says about a product comes from `apps/dashboard/src/lib/marketing.ts`.

## Layout

- One column for every page, its header and its footer: `WRAP` in `components/marketing/layout.ts`
  (90rem wide, gutters of 20, 32 and 48px). `H2` and `EYEBROW` there are the section headline and
  the mono label over it. Don't set a width or a gutter of your own.
- Home: the hero (see "The home page's hero"); every product as a bento grid; how roll works; five
  numbered chapters; the numbers; what shipped; and the closing over the dithered horizon. No two
  sections in a row share a composition (see "The home page's sections").
- `/products`: the catalogue. A directory of every product, then a section per family with the
  same cards (see "The product pages").
- A product page: eight sections, each with its own composition (see "The product pages").
- One idea and one main picture a section, and few words: the picture explains, the words name
  it. Nothing is added to look busy. When in doubt, remove.

## The header and the footer

`MarketingHeader` and `MarketingFooter` (`components/marketing/chrome.tsx`) are on every marketing
page: `/`, `/products`, `/products/<slug>` and `/brand`. A page never builds its own.

- What they link to, beside the products, is `components/marketing/nav.ts`. Every link there is a
  route or an anchor that exists (`ANCHORS`: the home page's `#products`, `#how`, `#measured` and
  `#shipped`). Add a page before adding its link.
- The bar is the Horizon logo, the Products and Developers menus, the products in `PINNED`, Sign in
  and the one primary button. At the top of a page it's clear, so a hero's backdrop runs under it;
  once the page has moved it takes a hairline and a blurred backdrop.
- A menu (`nav-panels.tsx`) opens as a band under the bar, one surface with it, and the page behind
  steps back. Both menus share one grid, four columns and a tile, so moving between them only
  changes the band's height (measured, animated; 220 ms).
  - Products: every product by family with its icon, name and `compat`, and a tile that draws the
    product being pointed at (its bento drawing, `what` and `insteadOf`). Until one is, it shows
    `FEATURED`.
  - Developers: where to start, what to read, what to wire in, the install line, and a terminal
    whose lines are the catalogue's own (`transcript()`).
  - The tiles are there from `xl`; between `lg` and `xl` the columns take the width.
- It opens on hover, a click, or the keyboard (Enter, Space, the down arrow). Tab goes from a
  trigger into its menu and out the other end to what follows in the bar; Escape closes it and
  gives the focus back to its trigger. A menu opened by hover stays open through the click that
  follows. Leaving the header closes it after 140 ms.
- Below `lg` the menus are one sheet under the bar (`MobileSheet`): every product by family, the
  developers' links, the install line, and Sign in and the primary button pinned at the bottom.
  The page under it doesn't scroll; it's `inert` while closed.
- Under reduced motion nothing slides or grows: menus and the sheet cut.
- The footer is the logo, one sentence and the install line, then a column per family and two for
  the rest of the site (`FOOTER`), over a line with the year and roll code's tagline.
- Panels are flat here like everywhere else: depth comes from the band's hairline and the page
  dimming, never a shadow.

## The home page's hero

`Hero` in `routes/index.tsx` is the first screen, centred: the announcement pill, the headline, one
paragraph, the primary button and the install line, over `HorizonScape` (the sun on the skyline,
the mask fading it out below), then the `Tour` with its tabs centred. Nothing sits between the
words and the tour: the scrolling strip of client names that used to follow it is gone, and the
products' protocols are said by the bento right after.

## The bento grid

`Bento` (`components/marketing/bento.tsx`) lays products out four columns wide (two below `xl`). An
item is a product's slug; `*slug` makes its card two columns wide. `columns={3}` is for a grid that
shares its row with something else (the catalogue's family rail).

- A card is the icon and name, one sentence (`what` bright, `insteadOf` muted) and the product's
  drawing. A wide card adds `compat` and its first three features, with the drawing on the right.
- On a phone the narrow cards sit two to a row without their drawings.
- A new product needs a drawing in `ART`; without one it still gets its card.

### Drawings

`components/marketing/art/` has one drawing per product, grouped by family, built from `kit.tsx`:

- `Art` is a 320×200 SVG (`w` and `h` change the sheet). Strokes don't scale with it, so lines
  stay hairlines at any card size.
- `Chip` is a pill with a mono label in it (a client, a command, a key) and `Tile` a rounded square
  with an icon (a resource). Both sit on `fill-panel`, so a wire can run behind them.
- Colours are token classes (`stroke-line-2`, `stroke-subtle`, `fill-fg`…). The accent marks one
  thing per drawing: what just happened (`Beacon`), or the one item that needs someone.
- `Dither` fills a shape with a halftone pattern at 25, 50 or 75%: that's how a drawing shades,
  never with a gradient.
- `T` is a mono label. Labels are things a person would type or read in the product (`GET cart:42`,
  `pr-1240`, `idle · 0 CU`), not captions.
- Motion is `flow` (dashes marching along a path) and `pulse`, both CSS. They run only while the
  card is on screen (`.art[data-live]`, in globals.css) and not at all under reduced motion, where
  the drawing is its own still frame.

A product's own page shows the same idea on a bigger sheet: `art/hero.tsx` maps each slug to a
560×400 drawing (`HERO`; `hero-data.tsx`, `hero-messaging.tsx`, `hero-build.tsx`,
`hero-platform.tsx`). The rules are the card drawings', plus:

- It's the card's picture with room to say more, not a second idea: Postgres is still branches on
  a time axis, now with each branch's compute and the wake.
- Every label is true of roll. A number comes from the product's `facts` (`awake in 0.66 s`,
  `resumes in 13 ms`), a hostname from the docs (`web-pr1240-shop.preview.roll.site`), a command
  from the CLI.
- A wire never crosses a label: start it past the text, or route it round.
- A new product needs an entry in `HERO`; without one its page shows the card drawing.

## The product pages

`/products/<slug>` is `routes/products/$product.tsx`: the sections in
`components/marketing/product-page.tsx`, in this order. No two in a row share a composition, and
each has one picture.

| Section | Composition | Picture |
| --- | --- | --- |
| `ProductHero` | words on the left, a framed stage on the right | the product's hero drawing, `compat` in the stage's footer |
| `ProductFacts` | three numbers across, split by hairlines | bars to scale under a fact with `against` |
| `ProductConnect` | a definition list beside a tabbed code panel | the code |
| `ProductDashboard` | centred heading over the full width | the real render, with the tour's camera |
| `ProductDemo` | a `bg-panel` band | the product's demo in a `Scene` |
| `ProductFeatures` | the heading pins on the left, a 2×2 grid on the right | each feature's icon |
| `ProductStart` | words beside a terminal | the product's terminal, typed |
| `ProductRelated` | a row of cards | the family's other products |

- The hero's headline is the product's `tagline`; its name sits over it with its icon. On a phone
  the stage shows the card drawing instead, whose labels are still readable at that width.
- `ProductFacts` counts each number up once. A fact with `against` draws both sides in one unit
  (`Versus`), named as the benchmark's doc names them; `factsNote` says where it was measured.
- `ProductConnect` is the compatibility story: `headline` in one line, then what it speaks, what
  connects and what it replaces, beside `CodePanel` (`code-panel.tsx`).
  - The snippets are `lib/marketing-connect.ts` (`SNIPPETS`, two or three a product): the CLI, then
    the clients people already have. Each is taken from a README or a doc named in the comment
    over it. Never write one from memory: a flag that doesn't exist is worse than no snippet.
  - The panel tints with the tokens only (comments, strings, keywords), and keeps a minimum
    height so a short tab doesn't move the page.
- `ProductDashboard` and `ProductDemo` appear only for a product that has a render (`shotFor`) or
  a demo (`visual`).
- `ProductFeatures` numbers the four `features`, each with its `icon`, and says what isn't there
  yet (`status`) beside them, not in small print at the bottom.

`/products` (`routes/products/index.tsx`, `components/marketing/catalogue.tsx`):

- The hero is centred over `HorizonScape` without its sun, the skyline under the install line.
- `Directory` overlaps it: one frame, a column per family, every product with its icon, name and
  `compat`. It's the whole catalogue on one screen, and each family's heading jumps to its section.
- `Families` is a section per family: a rail that pins on the left (its number, name, `lede` and
  the four families as a contents list) and the family's cards in a three-column `Bento` on the
  right. Which cards are wide is `LAYOUT` there.
- It closes on the install line.

A product in `lib/marketing.ts` has, beside what the cards use: `headline`, `facts` (with
`against` where there's a comparison in the docs), `features` (each with an `icon`), `status`,
`scenes` (the terminal's) and `visual` (which demo).

## Screenshots and the tour

- `apps/dashboard/scripts/marketing-shots` renders the real app at 1360×850 (3×) against made-up
  data (Playwright answers every request to rolld from `fixtures.mjs`) and writes
  `src/assets/marketing/dash-<name>.webp`, imported through Vite (never `public/`: every file there
  is a reserved owner name). Never draw a mock of a dashboard page. Run it again after a page it
  covers changes, and stop its server afterwards.
- `components/marketing/shots.ts` lists them. Each has its tab label and its `beats`: the stops of
  the tour on that page. A beat is a title, a sentence, and a `focus`, the rectangle it looks at
  in the render's own pixels (`[x, y, width, height]` of 1360×850). The first beat has no focus:
  the whole page. After re-rendering a page whose layout moved, measure its rectangles again.
- `Tour` (`components/marketing/shot.tsx`) shows a page at a time and moves a camera in on each
  beat's part, an accent ring around it and the rest dimmed, with the beat's words in a callout.
  - `camera()` fits the part in the window, never past twice the dashboard's own size (which is
    why the renders are 3×: still sharp on a 2× screen) and never past the picture's edge.
  - The callout lies over the picture, and moves out of the way when the part in focus is under
    it. Below 560px it sits under the picture.
  - It goes on by itself while on screen and while the callout isn't hovered or focused. The tabs
    pick a page, the callout's bars a stop. Under reduced motion nothing moves by itself and the
    camera cuts.
  - A page's picture is fetched when it's next, so the home page loads one render, not eight.
  - With one page (a product's own) there are no tabs. With several, the tabs sit on the left
    over the frame and a link into the dashboard on the right (from `lg`).

## Backdrops

Two canvases, both in theme inks (`inksOf`), both reacting to the pointer like `DotField`, paused
offscreen, drawn once under reduced motion, and following the shared mark state:

- `HorizonScape` (`horizon-scape.tsx`): the dotted horizon behind the heroes: the skyline a fraction
  of the height (`horizon`), and Horizon's three bands in dots as a sun in the middle (`sun`).
- `DitherScape` (`dither-scape.tsx`): the closing. Horizon's three bands (`horizonBands()` from
  `@roll/brand`, never redrawn) rise over a ridge, the whole scene ordered-dithered: a brightness
  field per 2px cell, compared with a 4×4 Bayer matrix, so each cell is ink or nothing. It's plain
  Canvas2D (the field is written into a small `ImageData`, scaled up without smoothing, and cut to
  round dots): no shader and no dependency. The land is in the muted ink and the mark in the
  foreground's, so the scene works in light mode; the accent appears only where the pointer or a
  ripple is.

Keep a backdrop behind or below the words and the screenshots, never under dense text.

## The home page's sections

Each has its own shape, heading treatment and background, so scrolling moves through different
rooms. `components/marketing/chapters.tsx` holds the chapters; `routes/index.tsx` the rest.

| Section | Composition | Heading |
| --- | --- | --- |
| How it works | the request diagram across a wider column than the rest of the page | centred |
| 01 Data | a story: the left column pins, three steps scroll past on the right | in the pinned column |
| 02 Messaging | a `bg-panel` band, the stream running edge to edge | centred, short |
| 03 Hosting | one frame split in two: the same traffic on an always-on fleet and on roll | left, with the link on the right |
| 04 roll code | a bento: the merge queue as the big tile, three small tiles with a drawing each | inside the big tile |
| 05 Agents | a `bg-panel` band, the canvas in a frame with a tabbed panel, the words beside it | beside the frame |
| Measured | rows: the number on the left, bars to scale across the rest | centred |
| Recently shipped | one row of cards with drawings; it scrolls sideways below `xl` | small, with a link |

- **How it works** (`how-it-works.tsx`) is the architecture as one request. Three columns joined
  by measured wires: the clients people already run with the protocol each speaks (`ROUTES`),
  `rolld` with a row per engine, and object storage with what each product keeps there. A product
  only gets a route if it keeps data in object storage. While the section is on screen an accent
  dot travels the client's wire (`along` in globals.css, on an `offset-path`), its engine goes from
  `asleep` to `waking` to `awake` (with the product's measured cold start, where
  `lib/marketing.ts` has one), an object lands in storage, the answer travels back, and the engine
  sleeps again a few requests later. The line under the diagram says the same in words. Under
  reduced motion it's one still frame.
- **01 Data** (`DataChapter`): the headline and the steps' names are `sticky`; an
  `IntersectionObserver` marks the step in the middle of the screen. The steps are the branch
  timeline, the `History` drawing (a branch shares its parent's objects) and the wake times on one
  second. On a phone nothing pins: the steps follow each other.
- **02 Messaging**: `StreamFlow` with `bleed`, which drops its frame and its steps and puts its
  counters in one strip under the canvas. Streams, Queues and Tasks are three links under it.
- **03 Hosting** (`scale-versus.tsx`): two canvases fed by one simulation. The left is a fleet
  sized for the busiest second that never sleeps; the right is roll, where an instance runs while
  something's asking. Each side counts the instance-seconds it bills, so the numbers pull apart;
  the buttons send requests to both.
- **04 roll code**: the tiles' drawings are `art/code-tiles.tsx` and `Agents` (numbers.tsx), built
  from the bento's kit; their words are roll code's `features` in `lib/marketing.ts`.
- **05 Agents**: `AgentsCanvas`, whose side panel has three tabs: the thread that's picked (the one
  waiting, at first), all the threads, and the machines.
- **Measured** (`numbers.tsx`): only measurements with something to stand beside. `Versus` draws a
  number against what it was measured against, `Span` a duration on an axis; each row prints its
  product's `factsNote`.
- **Recently shipped**: `Shipped`, with drawings from `art/shipped.tsx`.

`scene.tsx` has what the demos share: `Scene` (the frame a demo sits in, cut into panes by its own
hairlines), `Versus`, `Halftone` (the bento's `Dither` for bars and fills), `Steps` and `NOTE`.
Every grid starts from `grid-cols-[minmax(0,1fr)]`, so a wide child (a canvas, a command line)
can't push a section past a phone's width.

## Demos

`branch-timeline`, `stream-flow`, `scale-sim`, `scale-versus`, `merge-queue` and `agents-canvas`
follow AGENTS.md's motion rules: canvas, SVG or (for a handful of moving cards) DOM, paused
offscreen, a still frame under reduced motion. Each returns bare panes for a frame to hold, on the
home page and on its product's page ("Try it") alike:

- `branch-timeline`: what can branch and how, main's history with branches forking from where it
  was clicked, and the real CLI command for the newest branch (`cmd` on each kind follows
  `roll-cli`'s flags: Postgres takes `--at` as RFC 3339).
- `stream-flow`: producers, six partitions, sealed segments tiering to object storage and a
  consumer group. Framed (a product page), its `Steps` light one part at a time.
- `scale-sim` (the apps page): requests a second over four instance lanes, the wake from zero
  marked with its measured time, and what it bills beside a server that never sleeps.
- `merge-queue`: three columns (pushed, the queue, main) with the same cards moving across them
  (`motion`'s shared layout); the state is a pure reducer (`advance`), ticked only on screen. The
  batch area keeps three cards' height, so the page under it never jumps.
- `agents-canvas`: threads as draggable cards with their delegations and handoffs, and the tabbed
  panel. It's drawn in the marketing page's own tokens, not the agents surface's, so it can't be
  mistaken for a screenshot.
