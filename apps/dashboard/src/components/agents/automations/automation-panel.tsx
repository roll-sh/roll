"use client";

/**
 * One automation in the side panel (`#agents/automations/<id>`): its name, switch and Run in the
 * header, then Build (the builder, with Test), Runs (its history and a run's live detail) and
 * Settings (concurrency, limits, who it runs as, defaults, secrets). A new one (`…/new`, maybe
 * from a template) is the same panel with Create. Edits are a draft until saved; a draft is kept
 * while the page is open, so closing the panel loses nothing.
 */

import { Copy, FlaskConical, Link2, MoreHorizontal, Play, Plus, Trash2, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { Automation, AutomationSpec, Concurrency, TestEvent, TestResult } from "@/lib/automations-api";
import { TEMPLATES } from "@/lib/automation-templates";
import { setAgentsView, type AutomationsView } from "@/lib/agents-surface";
import { blankSpec, sameSpec, sampleEvent, specOf, triggerSource, triggerSummary, validate, type Problems, type Schema } from "@/lib/automations-ui";
import { ApiError } from "@/lib/code-api";
import { Menu } from "../menu";
import { IconButton } from "../session";
import { field as fieldClass } from "../settings-ui";
import { cn, Popover, PopoverPopup, PopoverTrigger, Segmented, Select, Switch } from "../ui";
import { useTemplateContext } from "./automations-view";
import { Builder } from "./builder";
import { ConfigForm } from "./config-form";
import { RunDetail, RunHistory } from "./runs";
import { PrimaryButton, QuietButton, Tile, triggerIcon, useAutomations, SizedSelect } from "./shared";

/** Drafts by automation id (`new:<template>` for new ones), kept while the page is open. */
const drafts = new Map<string, AutomationSpec>();
/** A webhook secret just made, by automation id: shown until the panel closes. */
const revealed = new Map<string, string>();

/** Forget every draft and shown secret (tests). */
export function forgetDrafts() {
  drafts.clear();
  revealed.clear();
}

export function AutomationPanel({ view, onClose }: { view: AutomationsView & { open: string }; onClose: () => void }) {
  const { automations, runs, store, catalog, loaded } = useAutomations();
  const isNew = view.open === "new";
  const automation = isNew ? null : (automations.get(view.open) ?? null);
  const ctx = useTemplateContext();
  const key = isNew ? `new:${view.template ?? ""}` : view.open;
  const base = useMemo<AutomationSpec>(() => {
    if (automation) return specOf(automation);
    const t = TEMPLATES.find((x) => x.id === view.template);
    return t ? t.build(ctx) : blankSpec();
    // The template is built once per opening, not again as machines come and go.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [automation, view.template]);
  const [draft, setDraftState] = useState<AutomationSpec>(() => drafts.get(key) ?? base);
  const setDraft = (next: AutomationSpec) => {
    setDraftState(next);
    drafts.set(key, next);
  };
  const dirty = isNew || !sameSpec(draft, base);
  // Someone else saved (another tab, the CLI): follow it unless there are edits here.
  const seenRevision = useRef(automation?.revision);
  const [stale, setStale] = useState(false);
  useEffect(() => {
    if (!automation || automation.revision === seenRevision.current) return;
    seenRevision.current = automation.revision;
    if (!drafts.has(key) || sameSpec(drafts.get(key)!, specOf(automation))) {
      drafts.delete(key);
      setDraftState(specOf(automation));
    } else setStale(true);
  }, [automation, key]);

  const [tried, setTried] = useState(!isNew);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [serverProblems, setServerProblems] = useState<Problems>({});
  const problems = useMemo(() => {
    const p = { ...validate(draft, catalog), ...serverProblems };
    if (!tried) {
      delete p.name;
      delete p.steps;
    }
    return p;
  }, [draft, catalog, serverProblems, tried]);
  const count = Object.keys(validate(draft, catalog)).length;

  const [test, setTest] = useState<{ event: TestEvent; text: string; result: TestResult | null; error: string | null; busy: boolean; open: boolean } | null>(null);
  const runTest = async (event?: TestEvent) => {
    if (!store) return;
    const ev = event ?? test?.event ?? sampleEvent(draft.trigger, catalog, Date.now(), draft.filters);
    setTest({ event: ev, text: test?.text ?? JSON.stringify(ev.data, null, 2), result: test?.result ?? null, error: null, busy: true, open: true });
    try {
      const result = await store.api.test(store.owner, { automation: draft, event: ev });
      setTest((t) => t && { ...t, result, busy: false });
    } catch (e) {
      setTest((t) => t && { ...t, error: e instanceof Error ? e.message : String(e), busy: false });
    }
  };

  const save = async () => {
    setTried(true);
    if (!store || count > 0) return;
    setSaving(true);
    setError(null);
    setServerProblems({});
    try {
      const saved = automation ? await store.api.update(store.owner, automation.id, draft, automation.revision) : await store.api.create(store.owner, draft);
      store.put(saved.automation);
      drafts.delete(key);
      seenRevision.current = saved.automation.revision;
      if (saved.webhook_secret) revealed.set(saved.automation.id, saved.webhook_secret);
      setStale(false);
      if (!automation) setAgentsView({ kind: "automations", list: view.list, open: saved.automation.id });
      else setDraftState(specOf(saved.automation));
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        setStale(true);
        setError("Someone saved a newer version while you edited.");
      } else {
        const msg = e instanceof Error ? e.message : String(e);
        const m = /^((?:name|trigger|filters|steps|rate_limit)[\w.]*): (.+)$/.exec(msg);
        if (m) setServerProblems({ [m[1]]: m[2] });
        setError(msg);
      }
    } finally {
      setSaving(false);
    }
  };
  const discard = () => {
    drafts.delete(key);
    setDraftState(base);
    setServerProblems({});
    setStale(false);
    setError(null);
  };
  const close = () => {
    if (automation) revealed.delete(automation.id);
    onClose();
  };

  const tab = view.tab ?? "build";
  const setTab = (t: "build" | "runs" | "settings") => setAgentsView({ kind: "automations", list: view.list, open: view.open, template: view.template, tab: t === "build" ? undefined : t });
  const runCount = automation ? [...runs.values()].filter((r) => r.automation === automation.id).length : 0;

  if (!isNew && !automation)
    return (
      <PanelFrame onClose={close} title={loaded ? "Not here any more" : "Loading…"} subtitle={loaded ? "It was deleted" : ""} icon={null}>
        <p className="p-6 text-[13px] text-muted">{loaded ? "Pick another automation from the list." : <span className="shimmer">Loading…</span>}</p>
      </PanelFrame>
    );

  const source = triggerSource(draft.trigger);
  return (
    <PanelFrame
      onClose={close}
      icon={<Tile icon={triggerIcon(source)} tone={draft.enabled ? "accent" : undefined} />}
      title={
        <input
          value={draft.name}
          onChange={(e) => setDraft({ ...draft, name: e.target.value })}
          placeholder="Name this automation"
          aria-label="Name"
          aria-invalid={problems.name ? true : undefined}
          className={cn("w-full min-w-0 rounded-md bg-transparent px-1 -mx-1 text-[14.5px] font-[590] tracking-[-0.02em] outline-none placeholder:text-faint hover:bg-fg/[0.03] focus:bg-fg/[0.04]", problems.name && "placeholder:text-danger/70")}
        />
      }
      subtitle={triggerSummary(draft.trigger, catalog)}
      tools={
        <>
          <span className="flex items-center gap-2 pr-1 text-[12px] text-muted">
            <Switch
              size="sm"
              checked={automation ? automation.enabled : draft.enabled}
              aria-label={(automation ? automation.enabled : draft.enabled) ? "Pause" : "Turn on"}
              onCheckedChange={async (on) => {
                if (!automation || !store) return setDraft({ ...draft, enabled: on });
                try {
                  store.put(on ? await store.api.enable(store.owner, automation.id) : await store.api.disable(store.owner, automation.id));
                } catch (e) {
                  setError(e instanceof Error ? e.message : String(e));
                }
              }}
            />
            <span className="max-sm:hidden">{(automation ? automation.enabled : draft.enabled) ? "On" : "Paused"}</span>
          </span>
          {automation && <RunButton automation={automation} manual={draft.trigger.type === "manual"} />}
          {automation && (
            <Menu
              label="More"
              trigger={<MoreHorizontal className="size-4" />}
              items={[
                {
                  label: "Duplicate",
                  icon: <Copy />,
                  onSelect: async () => {
                    if (!store) return;
                    const saved = await store.api.create(store.owner, { ...draft, name: `${draft.name} (copy)`, enabled: false });
                    store.put(saved.automation);
                    setAgentsView({ kind: "automations", open: saved.automation.id });
                  },
                },
                { label: "Copy link", icon: <Link2 />, onSelect: () => void navigator.clipboard?.writeText(`${window.location.origin}${window.location.pathname}${window.location.search}#agents/automations/${automation.id}`) },
                {
                  label: "Delete",
                  icon: <Trash2 />,
                  danger: true,
                  confirm: `Delete ${automation.name}?`,
                  onSelect: async () => {
                    if (!store) return;
                    await store.api.remove(store.owner, automation.id);
                    store.forget(automation.id);
                    drafts.delete(key);
                    setAgentsView({ kind: "automations", list: view.list });
                  },
                },
              ]}
            />
          )}
        </>
      }
    >
      {!isNew && (
        <div className="shrink-0 px-4 pt-3">
          <Segmented
            label="Show"
            value={tab}
            onChange={setTab}
            options={[
              { value: "build", label: "Build" },
              { value: "runs", label: <>Runs{runCount > 0 && <span className="ml-1 font-mono text-[10.5px] text-subtle tabular-nums">{runCount}</span>}</> },
              { value: "settings", label: "Settings" },
            ]}
          />
        </div>
      )}
      <div className="min-h-0 flex-1 overflow-y-auto px-4 pt-4 pb-6" data-scroll>
        {stale && (
          <p className="mb-3 flex items-center gap-2 rounded-lg border border-line-2 bg-raised px-3 py-2 text-[12px] text-muted">
            A newer version was saved elsewhere.
            <button type="button" className="ml-auto text-accent hover:underline" onClick={discard}>
              Load it
            </button>
          </p>
        )}
        {tab === "runs" && automation ? (
          view.run ? <RunDetail run={view.run} onBack={() => setAgentsView({ kind: "automations", list: view.list, open: view.open, tab: "runs" })} /> : <RunHistory automation={automation.id} />
        ) : tab === "settings" && automation ? (
          <SettingsTab draft={draft} setDraft={setDraft} automation={automation} />
        ) : (
          <>
            <Builder
              draft={draft}
              onChange={setDraft}
              catalog={catalog}
              problems={problems}
              automation={automation}
              revealed={automation ? (revealed.get(automation.id) ?? null) : null}
              onRotate={
                automation && store
                  ? async () => {
                      const saved = await store.api.rotateWebhookSecret(store.owner, automation.id);
                      store.put(saved.automation);
                      if (saved.webhook_secret) revealed.set(automation.id, saved.webhook_secret);
                      setDraftState((d) => ({ ...d }));
                    }
                  : undefined
              }
              test={test ? { result: test.result, data: test.event.data } : null}
            />
            {test?.open && (
              <TestCard
                test={test}
                onText={(text) => setTest({ ...test, text })}
                onRun={() => {
                  try {
                    const data: unknown = JSON.parse(test.text);
                    void runTest({ ...test.event, data });
                  } catch {
                    setTest({ ...test, error: "The event isn't valid JSON." });
                  }
                }}
                onReset={() => {
                  const ev = sampleEvent(draft.trigger, catalog, Date.now(), draft.filters);
                  setTest({ event: ev, text: JSON.stringify(ev.data, null, 2), result: null, error: null, busy: false, open: true });
                }}
                onClose={() => setTest(null)}
              />
            )}
          </>
        )}
      </div>
      {tab !== "runs" && (
        <footer className="flex shrink-0 items-center gap-2 border-t border-line px-4 py-2.5">
          <span className="min-w-0 flex-1 truncate text-[12px]">
            {error ? (
              <span className="text-danger" role="alert">
                {error}
              </span>
            ) : tried && count > 0 ? (
              <span className="text-danger">
                {count} thing{count === 1 ? "" : "s"} to fix
              </span>
            ) : saving ? (
              <span className="shimmer text-subtle">Saving…</span>
            ) : dirty && !isNew ? (
              <span className="text-muted">Unsaved changes</span>
            ) : !isNew ? (
              <span className="text-subtle">Saved · revision {automation?.revision}</span>
            ) : (
              <span className="text-subtle">{draft.enabled ? "It turns on when you create it." : "It stays paused when you create it."}</span>
            )}
          </span>
          {tab === "build" && (
            <QuietButton onClick={() => void runTest()} disabled={test?.busy} title="Try it with a sample event: nothing runs">
              <FlaskConical /> {test?.busy ? "Testing…" : "Test"}
            </QuietButton>
          )}
          {(isNew ? !sameSpec(draft, base) : dirty) && <QuietButton onClick={discard}>{isNew ? "Start over" : "Discard"}</QuietButton>}
          <PrimaryButton onClick={() => void save()} disabled={saving || (!dirty && !isNew)}>
            {isNew ? "Create" : "Save"}
          </PrimaryButton>
        </footer>
      )}
    </PanelFrame>
  );
}

function PanelFrame({ icon, title, subtitle, tools, onClose, children }: { icon: ReactNode; title: ReactNode; subtitle: string; tools?: ReactNode; onClose: () => void; children: ReactNode }) {
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-14 shrink-0 items-center gap-2.5 border-b border-line px-3">
        {icon}
        <div className="min-w-0 flex-1 leading-tight">
          {typeof title === "string" ? <p className="truncate text-[14px] font-[590] tracking-[-0.02em]">{title}</p> : title}
          <p className="truncate text-[11.5px] text-subtle">{subtitle}</p>
        </div>
        {tools}
        <IconButton label="Close (Esc)" onClick={onClose}>
          <X className="size-4" />
        </IconButton>
      </div>
      {children}
    </div>
  );
}

/** Run now; for a manual automation, with what to pass in (GitHub's "Run workflow" popover). */
function RunButton({ automation, manual }: { automation: Automation; manual: boolean }) {
  const { store } = useAutomations();
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const go = async () => {
    if (!store) return;
    setBusy(true);
    setError(null);
    let input: unknown = null;
    const t = text.trim();
    if (t) {
      try {
        input = JSON.parse(t);
      } catch {
        input = { prompt: t };
      }
    }
    try {
      const r = await store.api.run(store.owner, automation.id, { input });
      store.putRun(r);
      setOpen(false);
      setText("");
      setAgentsView({ kind: "automations", open: automation.id, tab: "runs", run: r.id });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger render={<button type="button" className="inline-flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-fg/[0.03] px-2.5 text-[12.5px] font-[510] text-soft inset-shadow-highlight transition-[background-color,border-color,color,scale] duration-150 hover:border-line-3 hover:bg-fg/[0.06] hover:text-fg active:scale-[0.97]" aria-label="Run now" />}>
        <Play className="size-3.5" /> Run
      </PopoverTrigger>
      <PopoverPopup side="bottom" align="end" sideOffset={6} aria-label="Run it now" className="w-80 p-3">
        <p className="text-[13px] font-[590] tracking-[-0.01em]">Run {automation.name}</p>
        <p className="mt-0.5 text-[12px] text-subtle">{manual ? "Give it a prompt, or JSON; templates read it as event.data.input." : "A run by hand; its conditions still apply."}</p>
        <textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              void go();
            }
          }}
          rows={3}
          aria-label="Input"
          placeholder={manual ? "Fix the flaky login test" : '{"note": "by hand"}'}
          className="mt-2.5 w-full resize-none rounded-lg border border-line-2 bg-panel-2 px-2.5 py-2 text-[13px] outline-none placeholder:text-faint focus:border-line-3"
        />
        {error && <p className="mt-1.5 text-[11.5px] text-danger">{error}</p>}
        <div className="mt-2.5 flex items-center justify-end gap-2">
          <span className="mr-auto text-[11px] text-subtle">⌘↵</span>
          <PrimaryButton onClick={() => void go()} disabled={busy}>
            <Play /> {busy ? "Starting…" : "Run now"}
          </PrimaryButton>
        </div>
      </PopoverPopup>
    </Popover>
  );
}

function TestCard({
  test,
  onText,
  onRun,
  onReset,
  onClose,
}: {
  test: { event: TestEvent; text: string; result: TestResult | null; error: string | null; busy: boolean };
  onText: (t: string) => void;
  onRun: () => void;
  onReset: () => void;
  onClose: () => void;
}) {
  const r = test.result;
  const runs = r ? r.steps.filter((s) => s.runs && !s.error).length : 0;
  return (
    <section aria-label="Test" className="rise mx-auto mt-6 w-full max-w-[620px] rounded-xl border border-line-2 bg-panel shadow-card inset-shadow-highlight">
      <div className="flex items-center gap-2 px-3.5 pt-3">
        <FlaskConical className="size-3.5 text-subtle" />
        <h3 className="text-[13px] font-[590] tracking-[-0.01em]">Test</h3>
        <span className="text-[12px] text-subtle">
          {test.event.source} · {test.event.kind}
        </span>
        <button type="button" onClick={onClose} aria-label="Close the test" className="ml-auto grid size-6 place-items-center rounded-md text-subtle hover:bg-fg/[0.06] hover:text-fg">
          <X className="size-3.5" />
        </button>
      </div>
      <div className="px-3.5 py-3">
        {r && (
          <p role="status" className={cn("mb-3 rounded-lg border px-3 py-2 text-[12.5px]", r.matched ? "border-ok/35 bg-ok/[0.06] text-soft" : "border-line-2 bg-fg/[0.02] text-muted")}>
            {r.matched
              ? `It would run: ${runs} of ${r.steps.length} step${r.steps.length === 1 ? "" : "s"}. What each template made is under its field.`
              : !r.trigger
                ? "The trigger doesn't take this event, so nothing would run."
                : `${r.filters.filter((f) => !f).length} condition${r.filters.filter((f) => !f).length === 1 ? "" : "s"} didn't pass, so nothing would run.`}
          </p>
        )}
        {test.error && <p className="mb-3 text-[12px] text-danger">{test.error}</p>}
        <label className="mb-1.5 block text-[12px] font-[510] text-soft">The event&apos;s data</label>
        <textarea value={test.text} onChange={(e) => onText(e.target.value)} rows={7} spellCheck={false} aria-label="Test event data" className="w-full resize-y rounded-lg border border-line-2 bg-panel-2 px-2.5 py-2 font-mono text-[11.5px] leading-[18px] outline-none focus:border-line-3" />
        <div className="mt-2 flex items-center justify-end gap-2">
          <button type="button" onClick={onReset} className="mr-auto text-[12px] text-muted hover:text-fg">
            A fresh sample
          </button>
          <QuietButton onClick={onRun} disabled={test.busy}>
            <FlaskConical /> {test.busy ? "Testing…" : "Test again"}
          </QuietButton>
        </div>
        <p className="mt-2 text-[11.5px] text-subtle">Nothing runs and no secret is filled in.</p>
      </div>
    </section>
  );
}

// ── settings ──

const CONCURRENCY: { value: Concurrency; label: string; hint: string }[] = [
  { value: "parallel", label: "Run alongside", hint: "Every event gets its own run." },
  { value: "skip", label: "Skip while one runs", hint: "An event while a run goes on is skipped." },
  { value: "queue", label: "Wait its turn", hint: "Runs go one after another." },
  { value: "cancel_previous", label: "Replace the running one", hint: "A new event cancels the run going on." },
];

const DEFAULTS_SCHEMA: Schema = {
  type: "object",
  properties: {
    target: { type: "object", title: "Runs on", format: "roll-target" },
    project: { type: "object", title: "Project", format: "roll-project" },
    model: { type: "string", title: "Model", format: "roll-model" },
    provider: { type: "string" },
    effort: { type: "string" },
    runtime_mode: { type: "string", title: "Access", enum: ["full_access", "auto_edit", "ask"], enumTitles: ["Full access", "Auto-edit", "Ask first"] },
    worktree: { type: "boolean", title: "Own worktree" },
  },
};

function Group({ title, children, note }: { title: string; children: ReactNode; note?: string }) {
  return (
    <section className="mb-6" aria-label={title}>
      <h3 className="mb-2 px-0.5 text-[12px] font-medium text-soft">{title}</h3>
      <div className="divide-y divide-line overflow-hidden rounded-xl border border-line-2 bg-raised shadow-card inset-shadow-highlight">{children}</div>
      {note && <p className="mt-1.5 px-0.5 text-[11.5px] text-subtle">{note}</p>}
    </section>
  );
}

function Row({ title, description, control, children }: { title: string; description?: string; control?: ReactNode; children?: ReactNode }) {
  return (
    <div className="px-3.5 py-3">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        <div className="min-w-44 flex-1">
          <h4 className="text-[13px] font-medium tracking-[-0.01em]">{title}</h4>
          {description && <p className="mt-0.5 text-[12px] leading-snug text-subtle">{description}</p>}
        </div>
        {control}
      </div>
      {children}
    </div>
  );
}

function SettingsTab({ draft, setDraft, automation }: { draft: AutomationSpec; setDraft: (s: AutomationSpec) => void; automation: Automation }) {
  const { store } = useAutomations();
  const conc = CONCURRENCY.find((c) => c.value === draft.concurrency) ?? CONCURRENCY[0];
  return (
    <div className="mx-auto w-full max-w-[620px]">
      <Group title="About">
        <Row title="Description" description="For you and whoever reads the list.">
          <textarea value={draft.description ?? ""} onChange={(e) => setDraft({ ...draft, description: e.target.value || undefined })} rows={2} aria-label="Description" className="mt-2 w-full resize-none rounded-lg border border-line-2 bg-panel-2 px-2.5 py-2 text-[13px] outline-none focus:border-line-3" />
        </Row>
      </Group>
      <Group title="Runs">
        <Row title="When one is already running" description={conc.hint} control={<SizedSelect width="w-52" label="When one is already running" value={draft.concurrency} onChange={(concurrency) => setDraft({ ...draft, concurrency })} options={CONCURRENCY.map((c) => ({ value: c.value, label: c.label }))} />} />
        <Row
          title="At most"
          description={draft.rate_limit ? undefined : "100 runs an hour unless you say otherwise."}
          control={
            <span className="flex items-center gap-2 text-[12.5px] text-muted">
              <input type="number" min={1} aria-label="Runs at most" value={draft.rate_limit?.max ?? ""} placeholder="100" onChange={(e) => setDraft({ ...draft, rate_limit: e.target.value === "" ? null : { max: Math.max(1, Number(e.target.value)), per_s: draft.rate_limit?.per_s ?? 3600 } })} className={cn(fieldClass, "w-20 font-mono text-[12.5px]")} />
              runs per
              <Segmented
                label="Per"
                value={String(draft.rate_limit?.per_s ?? 3600)}
                onChange={(v) => setDraft({ ...draft, rate_limit: { max: draft.rate_limit?.max ?? 100, per_s: Number(v) } })}
                options={[
                  { value: "3600", label: "hour" },
                  { value: "86400", label: "day" },
                ]}
              />
            </span>
          }
        />
      </Group>
      <Group title="Who it runs as" note="Agent tokens can run it by hand only when you allow it; they can never change it.">
        <Row
          title="Act as"
          description={draft.run_as ? `With the agent ${draft.run_as}'s narrower rights.` : "As you, the owner."}
          control={<input value={draft.run_as ?? ""} onChange={(e) => setDraft({ ...draft, run_as: e.target.value.trim() || null })} placeholder="You" aria-label="Act as agent" className={cn(fieldClass, "w-44 font-mono text-[12px]")} />}
        />
        <Row title="Agents may run it" description="Your agent tokens can start it by hand (MCP, the CLI)." control={<Switch checked={!!draft.allow_agents} onCheckedChange={(on) => setDraft({ ...draft, allow_agents: on || undefined })} aria-label="Agents may run it" />} />
      </Group>
      <Group title="Defaults for agent steps" note="A step's own settings win over these.">
        <div className="px-3.5 py-3.5">
          <ConfigForm
            schema={DEFAULTS_SCHEMA}
            value={(draft.defaults ?? {}) as Record<string, unknown>}
            onChange={(d) => setDraft({ ...draft, defaults: { target: null, project: null, provider: null, model: null, effort: null, runtime_mode: null, interaction_mode: null, worktree: null, ...d } as AutomationSpec["defaults"] })}
            ctx={{ variables: [], problems: {}, at: "defaults", secrets: [] }}
          />
        </div>
      </Group>
      <Secrets automation={automation} onChange={(a) => store?.put(a)} />
    </div>
  );
}

function Secrets({ automation }: { automation: Automation; onChange: (a: Automation) => void }) {
  const { store } = useAutomations();
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const add = async () => {
    if (!store) return;
    const n = name.trim().toUpperCase();
    if (!/^[A-Z0-9_]{1,64}$/.test(n)) return setError("Capital letters, digits and _.");
    if (!value) return setError("Give it a value.");
    setBusy(true);
    setError(null);
    try {
      await store.api.setSecret(store.owner, automation.id, n, value);
      store.put(await store.api.get(store.owner, automation.id));
      setName("");
      setValue("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Group title="Secrets" note="Sealed on the server and never shown again. Use them in a webhook step's headers as {{ secrets.NAME }}.">
      {automation.secrets.map((s) => (
        <Row
          key={s}
          title={s}
          control={
            <button
              type="button"
              onClick={async () => {
                if (!store) return;
                await store.api.removeSecret(store.owner, automation.id, s);
                store.put(await store.api.get(store.owner, automation.id));
              }}
              className="inline-flex h-7 items-center gap-1 rounded-md px-2 text-[12px] text-muted hover:bg-fg/[0.05] hover:text-danger"
              aria-label={`Remove the secret ${s}`}
            >
              <Trash2 className="size-3.5" /> Remove
            </button>
          }
        />
      ))}
      <div className="flex flex-wrap items-center gap-1.5 px-3.5 py-3">
        <input value={name} onChange={(e) => setName(e.target.value.toUpperCase())} placeholder="NAME" aria-label="Secret name" className={cn(fieldClass, "w-40 font-mono text-[12px]")} />
        <input type="password" value={value} onChange={(e) => setValue(e.target.value)} placeholder="Value" aria-label="Secret value" autoComplete="off" className={cn(fieldClass, "min-w-32 flex-1")} />
        <QuietButton onClick={() => void add()} disabled={busy}>
          <Plus /> Add
        </QuietButton>
        {error && <p className="w-full text-[11.5px] text-danger">{error}</p>}
      </div>
    </Group>
  );
}
