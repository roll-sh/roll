# roll code automations: the dashboard

The Automations view in the agents surface: the list, the templates, the builder, the run history
and run detail, and the notices. It follows the agents surface's design system
(docs/code/agents-design.md); this page covers what's specific to automations and why. The engine
and the wire are docs/code/automations.md.

Code: `apps/dashboard/src/components/agents/automations/` (views), `lib/automations-ui.ts`
(summaries, variables, validation, samples), `lib/automations-store.ts` (live state),
`lib/automation-templates.ts` (the gallery), `lib/automations-demo.ts` (`?demo`).

## Where it lives

- **Entry points.** The ⚡ button in the agents header (it toggles back to the canvas), "Automations",
  "New automation" and "Run automation…" in ⌘K, the automation chip on a thread's canvas card, a
  notice's "View the run", and links:
  `#agents/automations`, `…/runs`, `…/templates`, `…/new[/<template>]`, `…/<id>`,
  `…/<id>/runs[/<run>]`, `…/<id>/settings`.
- **List in the working area, detail in the side panel.** The list replaces the canvas inside the
  same inset panel (Linear's inverted L), and the open automation is the floating sheet beside it,
  the way a conversation sits beside the canvas.
  - It keeps one model for the whole surface: whatever you open slides in on the right, and Escape
    closes it (first the panel, then the list).
  - The list stays visible while you edit, so you can switch between automations without going
    back, and its live status keeps moving.
  - The builder is a vertical flow about 600px wide, which is the sheet's width. Zapier's editor
    is a narrow column for the same reason; a full-width canvas (n8n) suits branching graphs, and
    automations here are linear.
  - The canvas stays mounted underneath, so coming back finds it where it was.
- On a phone the panel covers the list, as conversations do.

## The list

| Part | What and why |
| --- | --- |
| Bar | A segmented control: Automations (count), Runs (an accent count while some run), Templates. Then a quiet filter field and the white "New automation". |
| Row | Two lines at 52px (T3's `ScheduledTaskRow` and Linear's list rows). Line 1 is the name at 13.5/510. Line 2 is the trigger in words plus "next in 3 h" (`Weekdays at 09:00 · Budapest · next in 10 h`). |
| Left | The trigger's icon on a tile. On narrow lists a 6px last-run dot sits on its corner; when the column has room the dot moves into the last-run column instead. |
| Columns | The last run (dot, state, `4m`), then seven days of runs as 4px tracks filled from the bottom (failed days red, Linear-quiet), then the count. Container queries drop columns as the side panel narrows the list. |
| End | T3's switch (on is the accent), then ⋯ (Run now, Duplicate, Delete with a confirm), which shows on hover, on focus and always on touch. |
| Paused | The row dims to 64% and line 2 reads "Paused" (T3). |
| Selected | A 2px accent bar on the left and the selected fill. |

Live state comes from `automation_run` events, not from `last_run`, which is only as fresh as the
last list (auto-core's note): `lastRunOf` takes the newest run the store has.

## Templates

- Cards in a one-, two- or three-column grid. Each card has the trigger's tile, an arrow, and the
  step kinds' icons in order: the automation's shape at a glance.
- The title is 13.5/590, followed by two lines of description, the trigger chip and "Connect
  first" for integrations that aren't set up. "Use →" appears on hover.
- A template builds a whole spec from the owner's real machines and projects
  (`AutomationTemplate.build(ctx)`), so it opens in the builder pointing at real things.
- Built-ins:
  - "Run: an agent in a worktree"
  - "Weekdays 9:00: triage new work"
  - "When main breaks: an agent fixes it"
  - "When a thread finishes: publish and land"
  - "Approve tests and builds"
  - "Webhook: an agent from anywhere"
  - "Tell me when an agent fails"
- Integration lanes add theirs to `TEMPLATES` with `integration` set.
- **Empty state.** With no automations yet the list is a calm hero ("Let your agents run on their
  own", the DotField fading under it), "Start from scratch", and the gallery. It follows Linear
  Loops' new-loop dialog, with templates first.

## The builder

- **Header.**
  - The name is edited in place: a borderless 14.5/590 field.
  - Under it, the trigger in words.
  - On the right: the switch (immediate for a saved automation), Run, ⋯ (Duplicate, Copy link,
    Delete) and Close.
  - Tabs Build · Runs · Settings sit under the header, as a segmented control.
- **Flow.** Cards joined by a 24px hairline. Hovering a connector shows a `+` that inserts a step
  there. The order is **When → Only if → Step 1…n → Add a step** (Linear Loops' order, Zapier's
  connectors).
  - Each card header has a small uppercase kicker ("When", "Only if", "Step 2"), then the title at
    13.5/590, the trigger or step icon on a tile, and the step id as a mono chip. The id is what
    later steps read as `steps.<id>.…`.
  - One step is open at a time, an accordion as in Zapier. A closed step reads as one line: where
    it runs, the model by name, worktree, the first template's text. After a test, that line shows
    the rendered text.
- **Trigger.** "Change" opens a menu of every trigger the catalog has. Integrations needing setup
  are marked "Connect". Each trigger type has its own body:

  | Trigger | Body |
  | --- | --- |
  | Schedule | Hourly, Daily, Weekdays, Weekly or Custom; a time field, a day for weekly, a raw cron for custom; T3's select of every IANA zone; the schedule in words |
  | Interval | A number with minutes, hours or days |
  | Webhook | The URL with Copy. A new secret shows once in an accent well ("shown this once") with Copy; after that only `rlh_••••1234` and "New secret". Then how to sign |
  | roll code | Event chips and repos |
  | Agents | Event chips |
  | Integration | Its events and its config form, from the catalog's JSON Schema |
  | By hand | Where it runs from (Run, the CLI command, MCP) and what `event.data.input` is |
- **Conditions** read as sentences: `If [field] [is ▾] [value] ×`, `and …`.
  - The field suggests the trigger's event fields as you type.
  - The condition menu holds each operator and its negation ("is", "is not", "doesn't contain",
    "is not set"…), so there's no separate NOT toggle.
  - After a test, each row shows Yes or No and the card says "All pass" or "Won't run".
- **Steps' forms are generated from the catalog's JSON Schemas** (`ConfigForm`). The convention,
  agreed with auto-core:
  - Properties render in their declared order.
  - `title`, `description`, `enum` (labels from `enumTitles`), `default`, `minimum`, `maximum`
    and `required` are honoured.
  - Strings are template fields, booleans T3 switches, and integers mono number fields.
  - Enums of up to three short labels become a segmented control; longer ones become T3's select.
  - Arrays of enums become chips, arrays of objects repeatable sub-forms (the respond step's
    rules), and string maps key/value rows (webhook headers).
  - `format` picks roll's pickers:

    | `format` | Picker |
    | --- | --- |
    | `template` / `template-long` | One-line or multi-line template field |
    | `roll-thread` | The owner's threads by title, grouped by project (the shared picker); a typed id or a variable too; empty means the run's latest thread |
    | `roll-machine` | The owner's machines and sandboxes; empty means the step's default |
    | `lookup` | The shared picker (`ui/picker.tsx`) over `lookup: {source, kind, parent?, extras?, extras_group?, create?}`: the integration's things from `GET …/lookup/{source}/{kind}` (one, or chips for an array), `parent` read from that sibling field when it holds one value (a create-issue step's states are its team's), `extras` first (`roll`, `No one`, the kinds of state), "Create label …" with `create`. Every lookup field also takes a typed value, and step fields a variable ("Use a variable"); the stored value stays the id, shown by name |
    | `roll-target` | A machine (T3 select with online, asleep or offline), a new sandbox (size, keep it), any online, or the project's machine |
    | `roll-project` | By project or by repository (cloned if missing) |
    | `roll-model` | The composer's model picker, from the target machine's providers (all machines' when the target isn't one machine); it writes `provider` and `effort` too, and those fields are hidden |
    | `roll-secret` | One of the automation's secrets |
    | `json` | A body: JSON whose strings are templates, or a template |

  - A built-in step whose catalog entry has no `properties` gets the dashboard's own schema
    (`BUILT_IN_STEPS`), so the builder works against an older server too.
- **Advanced**, folded under each step: name, id, "Run this step only if" (the same condition rows),
  "If it fails" (Stop the run / Go on), tries with the wait between them, and a time limit.
- **Template fields** (`TemplateInput`, Zapier's mapped pills and n8n's expressions in roll's
  restraint).
  - `{{ … }}` tokens show in the accent on a faint accent fill, drawn by a backdrop under a
    transparent textarea, so the caret and selection stay native.
  - Typing `{{` (or pressing the `{}` button) opens a glass list of what this step can read, in
    groups:
    - Event: the trigger's `fields` as `event.data.…`, plus `event.kind`, `event.source` and
      `event.id`
    - Run: `run.id`, `run.threads`, `automation.name`, `automation.id` and `owner`
    - one group per earlier step: its catalog `output` as `steps.<id>.…`
  - Each entry shows its sample value from the test event.
  - ↑↓ move, Enter or Tab inserts `{{ path }}` (closing an auto-typed `}}`), Esc closes.
  - Later steps are never offered. Validation also flags a reference to a later step, an unclosed
    `{{`, and `{{secrets.…}}` anywhere but a webhook's headers.
- **Validation** (`validate`) runs on every edit, and problems show under the field they're about.
  - For a new automation, "Give it a name" and "Add a step" wait for the first Create.
  - The footer counts what's left to fix. Save stays enabled and says why it didn't save.
  - A server 400 that names a path (`steps.2.config.prompt: …`) lands under that field.
  - A 409 (someone saved a newer revision) keeps your edits and offers "Load it".
- **Test** (the API's `TestRequest`) runs nothing and fills in no secret.
  - The sample event is the server's `EventInfo.sample` when it has one, else made from the event's
    fields. Either way it's then adjusted to meet the automation's own conditions, so the first test
    shows the run going.
  - The Test card shows the event's data as editable JSON, "A fresh sample", and one sentence of
    outcome: "It would run: 3 of 3 steps", or which condition stopped it.
  - Verdicts sit on the cards (Matches, All pass, Runs, Skipped, Error), and each template field
    shows what it rendered to underneath, in mono (n8n's preview).
- **Drafts.** Edits stay a draft until saved. A draft is kept while the page is open, so closing the
  panel loses nothing. "Discard" (or "Start over" for a new one) drops it.
- **Settings tab:**
  - Description.
  - "When one is already running": Run alongside, Skip, Wait its turn, or Replace the running one,
    each explained.
  - "At most N runs per hour/day".
  - "Act as" an agent, and "Agents may run it".
  - Defaults for agent steps (the same target, project and model pickers).
  - Secrets: names only; add with a password field; never shown again.

## Runs

- **History.** Rows of state (with a dot that breathes while running), the event's summary (or the
  reason it failed or was skipped), the steps as small icons coloured by state, the duration and how
  long ago. They follow GitHub Actions' run rows. The all-runs tab adds the automation's name and
  All / Failed / Running filters. "Older runs" pages back.
- **Detail** (GitHub Actions' steps, and T3's work log as a timeline).
  - **Header card.** The state, the total duration (live), and when it ran; the event's summary,
    and "Show the event" (its data as JSON); the reason when it failed. Retry and Cancel sit at the
    top right, only when they apply.
  - **Each step is one 32px line.** It shows an 18px state glyph on a 1px rail (the rail turns
    green under finished steps), the kind's icon, its title, its id in mono, "try 2" when it was
    retried, the state in words, the duration in mono and a chevron.
  - **Running, waiting and failed steps open themselves.**
    - The log shows lines at `+0.7s` offsets, errors in red, at most 384px high.
    - Then the output as `steps.<id>` JSON, and "Trying again in 30s" while retrying.
  - **Threads a step started** are chips under it (title · machine). A click opens the
    conversation in the side panel and reveals its card on the canvas.
- **Live.** `automation_run` events carry whole runs, and the store keeps the newer cursor, so a
  run's detail moves while you watch.

## On the canvas

- **Automation chip.** A thread an automation started gets a small chip (⚡ name) in its card's chip
  row; a click opens that run.
- **Source of the link.** The link comes from runs' `threads`, which the store indexes, because a
  thread record doesn't carry it. That is also why the store loads the first page of runs as soon as
  the canvas is shown.
- **Icon.** ⚡ (`Zap`) is the automations glyph everywhere. `Workflow` stays the orchestrator's, so
  the two never read as each other.

## Notices

- A `notify` step's `notice` is a toast: glass, 360px, the level's icon, the title at 13/510, up to
  three lines of body, and "Open" for its link (`#agents/…` links navigate the surface; http(s)
  links open a new tab) or "View the run".
- Toasts sit bottom-right, above the zoom bar, so they never cover the side panel's tools. They
  slide in from the edge over 0.5s on an ease-out quint (T3's toasts; static under reduced motion).
- At most three show at once. Each closes after 7s unless the pointer is on it.

## Live state

`AutomationsStore` (one per owner, `automationsFor(client, api)`):

- It loads the list, the first 200 runs and the catalog.
- It then follows the client's stream. `AgentsClient.onAutomation` gets `automation`,
  `automation_removed`, `automation_run` and `notice`, which never touch the canvas records.
- A record from a load and the same record from the stream are kept by the newer `cursor`, so a
  slow load never puts back something older.
- Removals are remembered with their cursor.
- Calls that answer with a record (save, run, cancel) put it in at once.

## Demo

`?demo` answers the whole automations API in memory (`DemoAutomations`, given the demo owner's
stream, machines and commands).

- **Seeded automations:** six, covering roll code (broken main), schedule (morning triage), agents
  (publish when done, approve tests), manual (run on ada-mbp) and a paused webhook.
- **History:** a week of runs.
- **Live run:** one run is going right now and finishes on its own about 24s in.
- **Running one:** "Run" starts a real thread on the demo canvas in a worktree, waits for it, and
  ends in a notice.
- **Test:** test runs fill templates and judge conditions.

## References

Researched on 2026-11-04 from public pages and T3 Code's source; the screenshots are in
`~/.local/share/roll-agents/shots/automations/refs/`.

- **Linear (Loops, triage rules).**
  - Loops' order (trigger, conditions, instructions, access), the webhook trigger showing its URL
    and secret, and two-line rows.
  - The new-loop dialog with templates.
  - "Run history" as sessions.
  - Taken: the flow's order, the templates-first empty state, the inverted L.
- **Zapier.**
  - The vertical editor of cards with `+` connectors.
  - Mapped data as pills with sample values; only earlier steps can be mapped.
  - Zap history's status filters.
  - Taken: the connectors, the accordion of steps, earlier steps only, sample values in the picker.
- **n8n.**
  - `{{ }}` expressions with a live result under the field.
  - Executions filtered by status; retry from the failed node.
  - Taken: the rendered value under each field after a test, and retry going on from the step that
    stopped.
- **GitHub Actions.**
  - "Run workflow" as a popover with inputs.
  - Run rows; steps as collapsible lines with durations and logs; re-run and cancel at the top right.
  - Taken: the Run popover (a prompt or JSON as `event.data.input`, ⌘↵), the timeline lines, and
    Retry and Cancel placement.
- **Vercel.** Deployment rows and build sections with durations. Cron Jobs as rows with Run.
  Taken: durations in mono beside each line.
- **Raycast.** Icon-led store cards. Taken: the template cards' icon chain, without the gradient.
- **T3 Code** (read from source).
  - `ScheduledTasksSettings.tsx`: row anatomy, the switch and ⋯, "Daily at 09:00 · Next run in 3h",
    paused rows dimmed.
  - `WorkLog.tsx`: the timeline's line height, indents, the 384px log cap, the chevron's rotation.
  - `toast.tsx`: 360px glass toasts, the 0.5s ease-out quint.
  - `settingsLayout.tsx`: the settings tab's grouped rows.
