/**
 * One automation's overview, the page between the list and its builder or runs: what starts it
 * and when it next runs, what it does (the steps in words), where it works, and its recent runs,
 * with the switch, Run, Edit and ⋯ in the header. Linear's issue page in spirit: read first, edit
 * one click away.
 */

import { ArrowRight, Copy, Link2, MoreHorizontal, Pencil, Trash2 } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import type { Automation, Catalog, Step } from "@/lib/automations-api";
import { setAgentsView, type AutomationsView } from "@/lib/agents-surface";
import { lastRunOf, runLabel, schemaOf, stepInfo, triggerSource, triggerSummary, weekOf } from "@/lib/automations-ui";
import { Menu } from "../menu";
import { ago } from "../shared";
import { cn, Switch } from "../ui";
import { useAutomationActions } from "./actions";
import { RunButton } from "./automation-panel";
import { RunRows, usePlaces } from "./automations-view";
import { MachineIcon, ProjectIcon } from "../identity";
import { useStepSummary } from "./builder";
import { SheetHeader, useSheet } from "./sheet";
import { inTime, QuietButton, RunDot, stepIcon, Tile, triggerIcon, useAutomations, WeekStrip } from "./shared";

const RECENT = 5;

export function Overview({ id, list }: { id: string; list?: AutomationsView["list"] }) {
  const { automations, runs, catalog, loaded } = useAutomations();
  const x = automations.get(id) ?? null;
  const sheet = useSheet();
  const actions = useAutomationActions(x, list);
  const [now] = useState(() => Date.now());
  const mine = useMemo(() => [...runs.values()].filter((r) => r.automation === id).sort((a, b) => b.created_at - a.created_at), [runs, id]);
  const week = useMemo(() => weekOf(runs.values(), id, now), [runs, id, now]);
  if (!x)
    return (
      <>
        <SheetHeader title={loaded ? "Not here any more" : "Loading…"} subtitle={loaded ? "It was deleted" : undefined} />
        <p className="p-6 text-[13px] text-muted">{loaded ? "Pick another automation from the list." : <span className="shimmer">Loading…</span>}</p>
      </>
    );
  const edit = () => setAgentsView({ kind: "automations", list, open: id, tab: "edit" });
  const last = lastRunOf(x, runs.values());
  return (
    <>
      <SheetHeader
        icon={<Tile icon={triggerIcon(triggerSource(x.trigger))} tone={x.enabled ? "accent" : undefined} />}
        title={x.name}
        subtitle={x.enabled ? triggerSummary(x.trigger, catalog) : "Paused"}
        tools={
          <span className="flex items-center gap-1.5">
            <Switch size="sm" checked={x.enabled} disabled={actions.busy === "toggle"} onCheckedChange={(on) => void actions.setEnabled(on)} aria-label={`${x.enabled ? "Pause" : "Turn on"} ${x.name}`} />
            <RunButton automation={x} manual={x.trigger.type === "manual"} list={list} />
            <QuietButton onClick={edit} ariaLabel="Edit">
              <Pencil /> <span className="max-sm:hidden">Edit</span>
            </QuietButton>
            <Menu
              label="More"
              trigger={<MoreHorizontal className="size-4" />}
              items={[
                { label: "Duplicate", icon: <Copy />, onSelect: () => void actions.duplicate() },
                { label: "Copy link", icon: <Link2 />, onSelect: () => void navigator.clipboard?.writeText(`${window.location.origin}${window.location.pathname}${window.location.search}#agents/automations/${x.id}`) },
                { label: "Delete", icon: <Trash2 />, danger: true, confirm: `Delete ${x.name}?`, onSelect: () => void actions.remove(() => sheet?.back()) },
              ]}
            />
          </span>
        }
      />
      <div className="min-h-0 flex-1 overflow-y-auto px-4 pt-4 pb-8" data-scroll>
        {actions.error && (
          <p role="alert" className="mb-3 text-[12px] text-danger">
            {actions.error}
          </p>
        )}
        {x.description && <p className="mb-4 text-[13px] leading-relaxed text-muted">{x.description}</p>}
        <dl className="grid grid-cols-3 overflow-hidden rounded-xl border border-line-2 bg-panel shadow-card inset-shadow-highlight max-sm:grid-cols-2" aria-label="At a glance">
          <Stat label="Last run">
            {last ? (
              <span className="flex items-center gap-1.5">
                <RunDot status={last.status} />
                <span className={cn(last.status === "failed" && "text-danger")}>{runLabel[last.status]}</span>
                <span className="text-subtle">{ago(last.at, now)}</span>
              </span>
            ) : (
              <span className="text-subtle">Never run</span>
            )}
          </Stat>
          <Stat label="Next run">{!x.enabled ? <span className="text-subtle">Paused</span> : x.next_at && x.next_at > now ? <span title={new Date(x.next_at).toLocaleString()}>{inTime(x.next_at, now)}</span> : <span className="text-subtle">On its trigger</span>}</Stat>
          <Stat label="Last 7 days">
            <WeekStrip {...week} />
          </Stat>
        </dl>

        <Section title="When">
          <div className="flex items-center gap-2.5 px-3.5 py-2.5">
            <Tile icon={triggerIcon(triggerSource(x.trigger))} />
            <span className="min-w-0 flex-1">
              <span className="block truncate text-[13px]">{triggerSummary(x.trigger, catalog)}</span>
              {x.filters.length > 0 && (
                <span className="block truncate text-[12px] text-subtle">
                  if {x.filters.length} condition{x.filters.length === 1 ? "" : "s"} pass{x.filters.length === 1 ? "es" : ""}: {x.filters.map((f) => `${f.not ? "not " : ""}${f.field.replace(/^event\.data\./, "")} ${f.op.replace("_", " ")}`).join(", ")}
                </span>
              )}
            </span>
          </div>
        </Section>

        <Section title="Steps" action={<SectionLink onClick={edit}>Edit steps</SectionLink>}>
          {x.steps.length ? (
            <ol aria-label="Steps">
              {x.steps.map((s, i) => (
                <StepLine key={s.id} step={s} index={i} catalog={catalog} onClick={edit} />
              ))}
            </ol>
          ) : (
            <p className="px-3.5 py-3 text-[12.5px] text-subtle">No steps yet.</p>
          )}
        </Section>

        <Where automation={x} />

        <Section
          title={`Recent runs${mine.length ? ` · ${mine.length}` : ""}`}
          action={mine.length > RECENT && <SectionLink onClick={() => setAgentsView({ kind: "automations", list, open: id, tab: "runs" })}>All runs</SectionLink>}
          bare
        >
          {mine.length ? (
            <div className="@container">
              <RunRows runs={mine.slice(0, RECENT)} showName={false} now={now} onOpen={(r) => setAgentsView({ kind: "automations", list, open: id, tab: "runs", run: r.id })} />
            </div>
          ) : (
            <p className="rounded-xl border border-dashed border-line-2 px-3.5 py-4 text-center text-[12.5px] text-subtle">No runs yet. Press Run, or wait for its trigger.</p>
          )}
        </Section>
      </div>
    </>
  );
}

function Stat({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0 border-line px-3.5 py-2.5 [&+&]:border-l max-sm:[&:nth-child(3)]:col-span-2 max-sm:[&:nth-child(3)]:border-t max-sm:[&:nth-child(3)]:border-l-0">
      <dt className="text-[11px] text-subtle">{label}</dt>
      <dd className="mt-1 flex h-5 items-center truncate text-[12.5px]">{children}</dd>
    </div>
  );
}

function Section({ title, action, bare, children }: { title: string; action?: ReactNode; bare?: boolean; children: ReactNode }) {
  return (
    <section className="mt-6" aria-label={title.split(" · ")[0]}>
      <div className="mb-2 flex items-center px-0.5">
        <h3 className="text-[12px] font-medium text-soft">{title}</h3>
        <span className="ml-auto">{action}</span>
      </div>
      {bare ? children : <div className="overflow-hidden rounded-xl border border-line-2 bg-panel shadow-card inset-shadow-highlight">{children}</div>}
    </section>
  );
}

function SectionLink({ onClick, children }: { onClick: () => void; children: ReactNode }) {
  return (
    <button type="button" onClick={onClick} className="inline-flex items-center gap-1 text-[12px] text-muted transition-colors hover:text-fg">
      {children} <ArrowRight className="size-3" />
    </button>
  );
}

function StepLine({ step: s, index, catalog, onClick }: { step: Step; index: number; catalog: Catalog; onClick: () => void }) {
  const info = stepInfo(catalog, s.kind);
  const summary = useStepSummary(s, schemaOf(info));
  const Icon = stepIcon(s.kind);
  return (
    <li className="border-line [&+&]:border-t">
      <button type="button" onClick={onClick} className="flex min-h-11 w-full items-center gap-2.5 px-3.5 py-2 text-left transition-colors duration-150 hover:bg-fg/[0.03]">
        <span className="w-4 shrink-0 text-right font-mono text-[11px] text-faint tabular-nums">{index + 1}</span>
        <span className="grid size-6 shrink-0 place-items-center rounded-md border border-line bg-fg/[0.02] text-subtle">
          <Icon className="size-3.5" strokeWidth={1.75} />
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[13px]">{s.name ?? info?.title ?? s.kind}</span>
          {summary && <span className="block truncate text-[12px] text-subtle">{summary}</span>}
        </span>
        {s.when.length > 0 && <span className="shrink-0 text-[11px] text-subtle">if {s.when.length}</span>}
        <code className="shrink-0 font-mono text-[10.5px] text-faint">{s.id}</code>
      </button>
    </li>
  );
}

function Where({ automation }: { automation: Automation }) {
  const { projects, machines } = usePlaces(automation);
  if (!projects.length && !machines.length) return null;
  return (
    <Section title="Works in">
      <ul className="flex flex-wrap gap-1.5 px-3.5 py-3">
        {projects.map((p) => (
          <li key={p.id} className="inline-flex h-6 items-center gap-1.5 rounded-md border border-line-2 bg-fg/[0.03] px-2 text-[12px] text-soft">
            <ProjectIcon project={p} className="size-3.5" />
            {p.name}
          </li>
        ))}
        {machines.map((m) => (
          <li key={m.id} className="inline-flex h-6 items-center gap-1.5 rounded-md border border-line-2 bg-fg/[0.03] px-2 text-[12px] text-soft">
            <MachineIcon machine={m} className="size-3.5" />
            {m.name}
          </li>
        ))}
      </ul>
    </Section>
  );
}
