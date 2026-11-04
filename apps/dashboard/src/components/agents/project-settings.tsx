"use client";

import { ArrowDownToLine, Bug, Check, CircleAlert, FlaskConical, Hammer, ListChecks, Pause, Play, Plus, Settings2, Trash2 } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import { type Machine, type Project, type ProjectScript, type ProjectSettings, type PullResult, reachable, type ScriptIcon, type ThreadDefaults } from "@/lib/agents-api";
import { btn, primary } from "../ui";
import { DefaultsRows } from "./machine-settings";
import { field, SettingRow, SettingsSection, type Setting } from "./settings-ui";
import { ago } from "./shared";
import { Select, Switch } from "./ui";
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

/** The project's rows (T3's project scope of the same settings). */
const P = {
  name: { id: "project.name", title: "Name", section: "Project", description: "How the project is called on the canvas." },
  autoPull: { id: "project.auto-pull", title: "Pull merged changes automatically", section: "Pull merged changes" },
  model: { id: "project.model", title: "Model", section: "New threads", description: "What a new agent in this project starts with. Default: the machine's, then yours." },
  permissions: { id: "project.permissions", title: "Permissions", section: "New threads" },
  mode: { id: "project.mode", title: "Plan first", section: "New threads" },
  workspace: { id: "project.workspace", title: "Workspace", section: "New threads" },
  scripts: {
    id: "project.scripts",
    title: "Scripts",
    section: "Scripts",
    description: "Commands for this checkout or a thread's worktree. A setup script runs in each new worktree, beside the agent.",
  },
} satisfies Record<string, Setting>;

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

  const settings: ProjectSettings = { ...project.settings, auto_pull: autoPull, defaults, scripts };
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

  const patchScript = (id: string, patch: Partial<ProjectScript>) => setScripts((all) => all.map((s) => (s.id === id ? { ...s, ...patch } : s)));

  return (
    <form
      className="text-[13px]"
      onSubmit={(e) => {
        e.preventDefault();
        if (changed && scriptsOk && online) void save();
      }}
    >
      <SettingsSection title="Name" ids={[P.name.id]}>
        <SettingRow
          s={P.name}
          control={<input id={`project-name-${project.id}`} aria-label="Name" value={name} onChange={(e) => setName(e.target.value)} className={`${field} w-56`} spellCheck={false} />}
        />
      </SettingsSection>

      <SettingsSection title="Pull merged changes" ids={[P.autoPull.id]}>
        <SettingRow
          s={{ ...P.autoPull, description: `Keeps ${branch} current: when a change lands, roll fast-forwards this checkout, but only while it's on ${branch} with no local changes or commits.` }}
          control={
            <>
              <button type="button" disabled={!online || busy !== null} onClick={() => act("pull", () => client.run(machine!.id, { type: "project.pull", project: project.id }, "pulled"))} className={btn}>
                <ArrowDownToLine className="size-3.5" />
                {busy === "pull" ? <span className="shimmer">Pulling…</span> : "Pull now"}
              </button>
              <Switch checked={autoPull} onCheckedChange={setAutoPull} aria-label="Pull merged changes automatically" />
            </>
          }
        >
          <PullLine pull={project.pull} className="mt-2" />
        </SettingRow>
      </SettingsSection>

      <SettingsSection title="New threads" ids={[P.model.id, P.permissions.id, P.mode.id, P.workspace.id]}>
        <DefaultsRows rows={P} defaults={defaults} providers={providers} disabled={false} onChange={setDefaults} />
      </SettingsSection>

      <SettingsSection title="Scripts" ids={[P.scripts.id]}>
        <SettingRow s={P.scripts}>
          {scripts.length > 0 && (
            <ul className="mt-2.5 space-y-2" aria-label="Scripts">
              {scripts.map((s) => (
                <li key={s.id} className="space-y-1.5 rounded-lg border border-line-2 bg-panel-2 p-2">
                  <div className="flex items-center gap-1.5">
                    <Select
                      label="Script icon"
                      className="w-28 min-w-0"
                      value={s.icon}
                      icon={<span className="grid size-4 place-items-center text-subtle [&>svg]:size-3.5">{SCRIPT_ICONS.find((i) => i.id === s.icon)?.icon}</span>}
                      onChange={(icon) => patchScript(s.id, { icon })}
                      options={SCRIPT_ICONS.map((i) => ({ value: i.id, label: i.label }))}
                    />
                    <input value={s.name} onChange={(e) => patchScript(s.id, { name: e.target.value })} placeholder="Name" aria-label="Script name" className={`${field} min-w-0 flex-1`} />
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
                    className={`${field} w-full font-mono text-[12px]`}
                  />
                  <label className="flex items-center gap-2 text-[12px] text-muted">
                    <Switch
                      size="sm"
                      checked={s.on_worktree}
                      aria-label="Run in each new worktree (setup)"
                      // One setup script: T3 runs the first one.
                      onCheckedChange={(on) => setScripts((all) => all.map((x) => ({ ...x, on_worktree: x.id === s.id ? on : on ? false : x.on_worktree })))}
                    />
                    Run in each new worktree (setup)
                  </label>
                </li>
              ))}
            </ul>
          )}
          <button type="button" onClick={() => setScripts((all) => [...all, { id: newScriptId(), name: "", command: "", icon: "play", on_worktree: false }])} className={`${btn} mt-2.5`}>
            <Plus className="size-3.5" /> Add a script
          </button>
        </SettingRow>
      </SettingsSection>

      <div className="flex items-center gap-1.5">
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
      {error && <p className="pt-2 text-[12px] text-danger">{error}</p>}
    </form>
  );
}
