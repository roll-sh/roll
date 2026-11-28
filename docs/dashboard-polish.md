# Dashboard polish

The user's notes on the dashboard (2026-11-28): the loading "sometimes just appears", the project
page "could be more clean", and the New agent popup "can look better". This file holds how each is
done now, so other pages follow the same rules, and a ranked `## Next` list of what's still to do.

## Loading

### What was wrong (current main, 2026-11-28)

These were recorded as frame sequences on a local rolld with fixture projects, with the API
answering normally and with every call held back by 1.5 s:

- **No feedback on a click.** When a page's code or data took a while, the old page stayed on
  screen with nothing showing that the click registered. Then the new page appeared all at once.
  Under load this lasted up to 4 s in development.
- **Blank frames.** On a first load the project page was an empty panel with no header until the
  workspace loaded. The secrets page body was blank until its environments arrived. The sidebar
  had no project rows.
- **Text placeholders.** About 100 places said "Loading…" or "Connecting to rolld…" in a shimmer,
  each in its own spot and size, and the content then replaced them in one frame.
- **Layout shift.** Home's "Secrets to fix" and "Recently deleted" arrived after the page showed
  and pushed the month's spend down.
- **Misleading values.** Home showed `$0.00` and `0 projects` while loading, and the sidebar
  showed `—`.

### The system (`apps/dashboard/src/components/loading.tsx`)

- **Navigations** keep the current page on screen. If a navigation is still pending after 150 ms:
  - every Horizon mark thinks (`RouteBusy` in `__root.tsx`, through `MarkStateProvider`);
  - the header's bottom border carries a short accent sweep (`NavProgress` in `AppShell`).

  A fast navigation shows neither.
- **A page's data**:
  - `Loaded` holds the content's place with a skeleton shaped like it. The skeleton fades in only
    after 150 ms (`.skeleton-in`), so a fast load shows nothing.
  - The content then arrives with `.arrive` (a 200 ms fade and a 4 px rise, ease-out).
  - Content that was already there when the page opened, such as the workspace store on a later
    visit, shows at once with no animation.
- **Skeletons** are built from `Skeleton` blocks in the page's own layout, so nothing moves when
  the content arrives. The shared shapes are:
  - `SkeletonList`: a list panel's rows;
  - `SkeletonCards`: a grid of cards;
  - `SkeletonLines`: rows inside a drawn panel;
  - `SkeletonHeading`;
  - `SkeletonCrumb`: a project's icon and name in the header.
- **The marks think** while anything loads: `useMarkBusy(active)` (`mark-state.tsx`) holds the
  thinking state, but only after 200 ms. A deploy or an answer that is playing wins over loading.
- **Shared placeholders now draw skeletons**, which covers most product pages without touching
  them:
  - `Connecting` in `components/ui.tsx`;
  - `Loading` in `tasks/ui.tsx` and `storage/iam/common.tsx`.
- **Late sections** that are "nothing when none" go below everything that is already there, and
  arrive with `.arrive`.
- **Reduced motion**: skeletons don't breathe, nothing fades or sweeps, and the marks draw their
  static frame for the state.

For a new page:
1. Wrap what waits for data in `<Loaded ready={…} skeleton={…}>`.
2. Build the skeleton from the same layout classes as the content.
3. Never write a "Loading…" line or a spinner for a page's first load.

Buttons that are working (`Saving…`, `Deploying…`) keep their text: that is an action's state, not
loading.

## Next

Ranked; the top item is in progress.

1. **The project page**: the empty state and the frame.
   - One obvious first action.
   - Secrets and environments easy to find.
   - Less in the header: the `Previews` tab and the `Previews` toggle are both there today.
   - The footer strip and the usage card.
2. **The New agent popup**: spacing, one control height, the title row's pickers, the key hints.
3. **The Horizon mark before hydration**: it's a canvas, so it's blank until the client's
   JavaScript runs. Render the static mark (`horizonSvg`) under the canvas until its first frame.
4. **Product detail pages**: replace the remaining tab-level "Loading…" lines with `Loaded` and
   skeletons, page by page as each product's thread touches them:
   - redis, nosql and search databases;
   - storage buckets;
   - apps services;
   - code repos.
