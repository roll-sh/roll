"use client";

import { ArrowDownToLine, Bug, Check, CircleAlert, FlaskConical, Hammer, ListChecks, Pause, Play, Plus, Settings2, Trash2, X } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import {
  type InteractionMode,
  type Machine,
  type Project,
  type ProjectScript,
  type ProjectSettings,
  type PullResult,
  type RuntimeMode,
  reachable,
  type ScriptIcon,
  type ThreadDefaults,
} from "@/lib/agents-api";
import { btn, input, primary } from "../ui";
import { ago, shortPath } from "./shared";
import { useAgents } from "./use-agents";

/**
 * Project settings, as T3 Code has them per project (`ProjectDefaultsSettings.tsx`,
 * `ProjectActionsSettings.tsx`, `projectScriptEditor.tsx` in apps/web there): auto-pull, what new
 * threads start with, and the project's scripts (the setup script runs in each new worktree).
 * Saving sends `project.update` with the settings whole; the record that comes back is the truth.
 */

export const SCRIPT_ICONS: { id: ScriptIcon; label: string; icon: ReactNode }[] = [
  { id: "play", label: "Run", icon: <Play /> },
  { id: "test", label: "Test", icon: <FlaskConical /> },
  { id: "lint", label: "Lint", icon: <ListChecks /> },
  { id: "configure", label: "Setup", icon: <Settings2 /> },
  { id: "build", label: "Build", icon: <Hammer /> },
  { id: "debug", label: "Debug", icon: <Bug /> },
];

const RUNTIME: { id: RuntimeMode; label: string }[] = [
  { id: "full_access", label: "Full access" },
  { id: "auto_edit", label: "Auto edit" },
  { id: "ask", label: "Ask" },
];

/** What the last pull did, in one quiet line; `null` when there is nothing to say. */
export function pullText(pull: PullResult | null, now = Date.now()): { text: string; tone: "ok" | "skip" | "fail" } | null {
  if (!pull) return null;
  const when = ago(pull.at, now);
  const o = pull.outcome;
  switch (o.status) {
    case "pulled":
      return { text: `Pulled ${o.commits} commit${o.commits === 1 ? "" : "s"} into ${o.branch} · ${when}`, tone: "ok" };
    case "up_to_date":
      return { text: `${o.branch} is up to date · ${when}`, tone: "ok" };
    case "skipped":
      return { text: `Didn't pull: ${o.message} · ${when}`, tone: "skip" };
    case "failed":
      return { text: `Pull failed: ${o.message} · ${when}`, tone: "fail" };
  }
}

/** The last pull's line, for the settings and the project's card. */
export function PullLine({ pull, className = "" }: { pull: PullResult | null; className?: string }) {
  const line = pullText(pull);
  if (!line) return null;
  const icon =
    line.tone === "fail" ? <CircleAlert className="size-3 shrink-0 text-danger" /> : line.tone === "skip" ? <Pause className="size-3 shrink-0" /> : pull?.outcome.status === "pulled" ? <ArrowDownToLine className="size-3 shrink-0" /> : <Check className="size-3 shrink-0" />;
  return (
    <p className={`flex min-w-0 items-center gap-1.5 text-[11.5px] text-subtle ${className}`} title={line.text}>
      {icon}
      <span className="truncate">{line.text}</span>
    </p>
  );
}

const newScriptId = () => `s${Math.random().toString(36).slice(2, 9)}`;

/** JSON with sorted keys and no unset fields: equal settings compare equal. */
const canon = (v: unknown) =>
  JSON.stringify(v, (_k, x: unknown) =>
    x && typeof x === "object" && !Array.isArray(x)
      ? Object.fromEntries(
          Object.entries(x)
            .filter(([, y]) => y !== undefined)
            .sort(([a], [b]) => a.localeCompare(b)),
        )
      : x,
  );

function Switch({ on, onChange, label }: { on: boolean; onChange: (on: boolean) => void; label: string }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      onClick={() => onChange(!on)}
      className={`relative h-[18px] w-8 shrink-0 rounded-full border transition-colors duration-150 ${on ? "border-accent bg-accent" : "border-line-2 bg-panel-2"}`}
    >
      <span className={`absolute top-[2px] size-3 rounded-full transition-[left] duration-150 ease-out ${on ? "left-[15px] bg-white" : "left-[2px] bg-subtle"}`} />
    </button>
  );
}

function Section({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <section className="space-y-2.5 border-t border-line px-4 py-4">
      <div>
        <h3 className="text-[13px] font-medium tracking-[-0.01em]">{title}</h3>
        {hint && <p className="mt-0.5 text-[12px] text-subtle">{hint}</p>}
      </div>
      {children}
    </section>
  );
}

function Segmented<T extends string>({ label, value, options, onChange }: { label: string; value: T; options: { id: T; label: string }[]; onChange: (v: T) => void }) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex flex-wrap gap-0.5 rounded-lg border border-line bg-bg p-0.5 text-[12px]">
      {options.map((o) => (
        <button
          key={o.id}
          type="button"
          role="radio"
          aria-checked={value === o.id}
          onClick={() => onChange(o.id)}
          className={`rounded-md px-2 py-1 transition ${value === o.id ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
      <span className="w-24 shrink-0 text-[12px] text-muted">{label}</span>
      {children}
    </div>
  );
}

/** The settings of one project, edited and saved whole. `onDone` after a save or a cancel. */
export function ProjectSettingsForm({ project, machine, onDone }: { project: Project; machine: Machine | undefined; onDone?: () => void }) {
  const { client } = useAgents();
  const online = reachable(machine);
  const providers = useMemo(() => (machine?.info?.providers ?? []).filter((p) => p.installed), [machine]);
  const [name, setName] = useState(project.name);
  const [autoPull, setAutoPull] = useState(project.settings.auto_pull);
  const [defaults, setDefaults] = useState<ThreadDefaults>(project.settings.defaults);
  const [scripts, setScripts] = useState<ProjectScript[]>(project.settings.scripts);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const saved = useMemo(() => new Set(project.settings.scripts.map((s) => s.id)), [project.settings.scripts]);

  const settings: ProjectSettings = { auto_pull: autoPull, defaults, scripts };
  const changed = name.trim() !== project.name || canon(settings) !== canon(project.settings);
  const scriptsOk = scripts.every((s) => s.name.trim() && s.command.trim());
  const branch = project.git?.branch && project.git.branch !== "HEAD" ? project.git.branch : "the default branch";

  const act = async (key: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    setError(null);
    try {
      await fn();
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setBusy(null);
    }
  };
  const save = () =>
    act("save", () =>
      client.run(machine!.id, { type: "project.update", project: project.id, name: name.trim() !== project.name ? name.trim() : undefined, settings }, "ok"),
    ).then((ok) => ok && onDone?.());

  const provider = providers.find((p) => p.id === defaults.provider);
  const setDefault = (patch: Partial<ThreadDefaults>) =>
    setDefaults((d) => {
      const next: ThreadDefaults = { ...d, ...patch };
      for (const k of Object.keys(next) as (keyof ThreadDefaults)[]) if (next[k] === undefined) delete next[k];
      return next;
    });
  const patchScript = (id: string, patch: Partial<ProjectScript>) => setScripts((all) => all.map((s) => (s.id === id ? { ...s, ...patch } : s)));

  return (
    <form
      className="text-[13px]"
      onSubmit={(e) => {
        e.preventDefault();
        if (changed && scriptsOk && online) void save();
      }}
    >
      <div className="space-y-1.5 px-4 pb-4">
        <label className="block text-[12px] text-muted" htmlFor={`project-name-${project.id}`}>
          Name
        </label>
        <input id={`project-name-${project.id}`} value={name} onChange={(e) => setName(e.target.value)} className={`${input} w-full`} spellCheck={false} />
        <p className="truncate font-mono text-[11px] text-subtle" title={project.path}>
          {project.repo ? `${project.repo.owner}/${project.repo.repo} · ` : ""}
          {shortPath(project.path, machine?.info?.home)}
        </p>
      </div>

      <Section title="Pull merged changes" hint={`Keeps ${branch} current: when a change lands, roll fast-forwards this checkout, but only while it's on ${branch} with no local changes or commits.`}>
        <div className="flex items-center gap-3">
          <Switch on={autoPull} onChange={setAutoPull} label="Pull merged changes automatically" />
          <span className="text-[12.5px] text-muted">{autoPull ? "On" : "Off"}</span>
          <button
            type="button"
            disabled={!online || busy !== null}
            onClick={() => act("pull", () => client.run(machine!.id, { type: "project.pull", project: project.id }, "pulled"))}
            className={`${btn} ml-auto`}
          >
            <ArrowDownToLine className="size-3.5" />
            {busy === "pull" ? <span className="shimmer">Pulling…</span> : "Pull now"}
          </button>
        </div>
        <PullLine pull={project.pull} />
      </Section>

      <Section title="New threads" hint="What a new agent in this project starts with. Default keeps the composer's own choice.">
        <Row label="Provider">
          <select
            aria-label="Default provider"
            value={defaults.provider ?? ""}
            onChange={(e) => setDefault({ provider: e.target.value || undefined, model: undefined, effort: undefined })}
            className={`${input} py-1 text-[12.5px]`}
          >
            <option value="">Default</option>
            {providers.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
            {defaults.provider && !provider && <option value={defaults.provider}>{defaults.provider} (not on this machine)</option>}
          </select>
          {provider && provider.models.length > 0 && (
            <select aria-label="Default model" value={defaults.model ?? ""} onChange={(e) => setDefault({ model: e.target.value || undefined })} className={`${input} py-1 text-[12.5px]`}>
              <option value="">Default model</option>
              {provider.models.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.name}
                </option>
              ))}
            </select>
          )}
        </Row>
        <Row label="Workspace">
          <Segmented
            label="Where new threads work"
            value={defaults.worktree === undefined ? "default" : defaults.worktree ? "worktree" : "checkout"}
            options={[
              { id: "default", label: "Default" },
              { id: "checkout", label: "This checkout" },
              { id: "worktree", label: "New worktree" },
            ]}
            onChange={(v) => setDefault({ worktree: v === "default" ? undefined : v === "worktree" })}
          />
        </Row>
        <Row label="Permissions">
          <Segmented
            label="Permissions for new threads"
            value={defaults.runtime_mode ?? "default"}
            options={[{ id: "default", label: "Default" }, ...RUNTIME]}
            onChange={(v) => setDefault({ runtime_mode: v === "default" ? undefined : (v as RuntimeMode) })}
          />
        </Row>
        <Row label="Mode">
          <Segmented
            label="Interaction mode for new threads"
            value={defaults.interaction_mode ?? "default_mode"}
            options={[
              { id: "default_mode", label: "Default" },
              { id: "plan", label: "Plan first" },
            ]}
            onChange={(v) => setDefault({ interaction_mode: v === "plan" ? ("plan" as InteractionMode) : undefined })}
          />
        </Row>
      </Section>

      <Section title="Scripts" hint="Commands for this checkout or a thread's worktree. A setup script runs in each new worktree, beside the agent.">
        {scripts.length > 0 && (
          <ul className="space-y-2" aria-label="Scripts">
            {scripts.map((s) => (
              <li key={s.id} className="space-y-1.5 rounded-lg border border-line bg-bg p-2">
                <div className="flex items-center gap-1.5">
                  <span className="grid size-6 shrink-0 place-items-center text-subtle [&>svg]:size-3.5">{SCRIPT_ICONS.find((i) => i.id === s.icon)?.icon}</span>
                  <input value={s.name} onChange={(e) => patchScript(s.id, { name: e.target.value })} placeholder="Name" aria-label="Script name" className={`${input} min-w-0 flex-1 py-1 text-[12.5px]`} />
                  <select value={s.icon} onChange={(e) => patchScript(s.id, { icon: e.target.value as ScriptIcon })} aria-label="Script icon" className={`${input} py-1 text-[12px]`}>
                    {SCRIPT_ICONS.map((i) => (
                      <option key={i.id} value={i.id}>
                        {i.label}
                      </option>
                    ))}
                  </select>
                  {saved.has(s.id) && (
                    <button
                      type="button"
                      title={`Run ${s.name} in a terminal`}
                      aria-label={`Run ${s.name}`}
                      disabled={!online || busy !== null}
                      onClick={() => act(`run:${s.id}`, () => client.run(machine!.id, { type: "terminal.open", project: project.id, script: s.id, cols: 100, rows: 26 }, "created"))}
                      className="grid size-7 place-items-center rounded-md text-subtle transition hover:bg-panel-2 hover:text-fg disabled:opacity-30"
                    >
                      <Play className="size-3.5" />
                    </button>
                  )}
                  <button
                    type="button"
                    title="Remove"
                    aria-label={`Remove ${s.name || "script"}`}
                    onClick={() => setScripts((all) => all.filter((x) => x.id !== s.id))}
                    className="grid size-7 place-items-center rounded-md text-subtle transition hover:bg-panel-2 hover:text-danger"
                  >
                    <Trash2 className="size-3.5" />
                  </button>
                </div>
                <input
                  value={s.command}
                  onChange={(e) => patchScript(s.id, { command: e.target.value })}
                  placeholder="pnpm test"
                  aria-label="Script command"
                  spellCheck={false}
                  className={`${input} w-full py-1 font-mono text-[12px]`}
                />
                <label className="flex items-center gap-1.5 text-[12px] text-muted">
                  <input
                    type="checkbox"
                    checked={s.on_worktree}
                    // One setup script: T3 runs the first one.
                    onChange={(e) => setScripts((all) => all.map((x) => ({ ...x, on_worktree: x.id === s.id ? e.target.checked : e.target.checked ? false : x.on_worktree })))}
                    className="accent-[var(--accent)]"
                  />
                  Run in each new worktree (setup)
                </label>
              </li>
            ))}
          </ul>
        )}
        <button type="button" onClick={() => setScripts((all) => [...all, { id: newScriptId(), name: "", command: "", icon: "play", on_worktree: false }])} className={btn}>
          <Plus className="size-3.5" /> Add a script
        </button>
      </Section>

      <div className="flex items-center gap-1.5 border-t border-line px-4 pt-4">
        <button type="submit" disabled={!changed || !scriptsOk || !online || busy !== null || !name.trim()} className={primary}>
          {busy === "save" ? <span className="shimmer">Saving…</span> : "Save"}
        </button>
        {onDone && (
          <button type="button" onClick={onDone} className={btn}>
            Cancel
          </button>
        )}
        {!online && <span className="ml-auto text-[12px] text-subtle">{machine?.name ?? "The machine"} is offline</span>}
      </div>
      {error && <p className="px-4 pt-2 text-[12px] text-danger">{error}</p>}
    </form>
  );
}
