"use client";

import { ArrowDownToLine, Bug, Check, CircleAlert, Eraser, FlaskConical, Hammer, ListChecks, Pause, Play, Plus, Settings2, Trash2 } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import {
  type AutoSettle,
  type Cleanup,
  type CleanupReport,
  type IdentityColor,
  type KeptReason,
  type Machine,
  type Project,
  type ProjectIcon as ProjectIconSetting,
  type ProjectScript,
  type ProjectSettings,
  type PullResult,
  reachable,
  type ScriptIcon,
  type ThreadDefaults,
} from "@/lib/agents-api";
import { checkProjectIcon, projectColor } from "@/lib/agents-identity";
import { ColorPicker, ProjectIcon } from "./identity";
import { btn, primary } from "../ui";
import { DefaultsRows } from "./machine-settings";
import { field, SettingRow, SettingsSection, type Setting } from "./settings-ui";
import { ago } from "./shared";
import { Select, Switch } from "./ui";
import { useEscapeLayer } from "./escape-layers";
import { useLeaveGuard } from "@/lib/agents-surface";
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
  name: { id: "project.name", title: "Name", section: "Name and icon", description: "How the project is called on the canvas." },
  color: {
    id: "project.color",
    title: "Colour",
    section: "Name and icon",
    description: "Its cards, edges and rows carry it everywhere. Auto is made from its repo or name, so it's the same on every device and machine.",
    terms: ["color", "colour", "tint", "identity"],
  },
  icon: {
    id: "project.icon",
    title: "Icon",
    section: "Name and icon",
    description: "Its own logo when roll finds one in the repo (a favicon, an app icon or a logo), else its monogram.",
    terms: ["logo", "favicon", "monogram", "emoji", "picture", "identity"],
  },
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
  autoSettle: {
    id: "project.auto-settle",
    title: "Settle inactive threads",
    section: "Settling and cleanup",
    description: "After this long without a message or a run, a thread moves to the settled shelf. A new message brings it back.",
    terms: ["auto-settle", "auto settle", "inbox", "shelf", "inactive", "days"],
  },
  onLand: { id: "project.settle-on-land", title: "Settle when its change lands", section: "Settling and cleanup", description: "A closed change always settles its thread.", terms: ["merged", "pr", "landed"] },
  keepChanges: {
    id: "project.keep-changes",
    title: "Keep threads with an open change",
    section: "Settling and cleanup",
    description: "A thread published to roll code doesn't settle for inactivity until its change lands or closes.",
    terms: ["pull request", "pr", "keep"],
  },
  cleanup: {
    id: "project.cleanup",
    title: "Remove settled worktrees",
    section: "Settling and cleanup",
    description: "The worktree of a thread settled or archived this long ago is removed. Its branch and checkpoints stay, and its next message puts it back.",
    terms: ["worktree", "cleanup", "clean up", "disk", "storage", "delete"],
  },
  cleanupKeep: {
    id: "project.cleanup-keep",
    title: "Keep a worktree that has",
    section: "Settling and cleanup",
    description: "A worktree with uncommitted changes always stays. Ignored files are things like .env, build output and node_modules.",
    terms: ["uncommitted", "ignored", "local files"],
  },
} satisfies Record<string, Setting>;

const SETTLE_DAYS = [1, 2, 3, 5, 7, 14, 30, 90];
const CLEANUP_DAYS = [0, 1, 3, 7, 14, 30, 90];
const days = (n: number) => (n === 0 ? "Right away" : n === 1 ? "After 1 day" : `After ${n} days`);
const dayOptions = (all: number[], current: number | null) =>
  [{ value: "never", label: "Never" }, ...[...new Set([...all, ...(current === null ? [] : [current])])].sort((a, b) => a - b).map((n) => ({ value: String(n), label: days(n) }))];

const KEPT_REASON: Record<KeptReason, string> = {
  local_changes: "local changes",
  busy: "still working",
  terminal: "a terminal running in it",
  other_branch: "another branch checked out",
  shared: "another thread works in it",
  not_managed: "not a worktree roll made",
  failed: "git refused",
};

/** What "Clean up now" did, in a line: "Removed 2 worktrees · kept 1 (local changes)". */
export function cleanupText(r: CleanupReport): string {
  const plural = (n: number) => `${n} worktree${n === 1 ? "" : "s"}`;
  if (r.removed.length === 0 && r.kept.length === 0) return "Nothing to clean up: no settled or archived thread has a worktree.";
  const why = [...new Set(r.kept.map((k) => KEPT_REASON[k.reason]))].join(", ");
  const kept = r.kept.length ? `kept ${r.kept.length} (${why})` : "";
  const removed = r.removed.length ? `Removed ${plural(r.removed.length)}` : "";
  return [removed, kept].filter(Boolean).join(" · ").replace(/^kept/, "Kept");
}

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

const ICON_KINDS: { value: ProjectIconSetting["kind"]; label: string }[] = [
  { value: "auto", label: "Logo, else monogram" },
  { value: "monogram", label: "Monogram" },
  { value: "emoji", label: "Emoji" },
  { value: "file", label: "A picture in the project" },
];

/** The icon: a live preview, how it's made, and the letters, emoji or path for it. */
function IconEditor({
  project,
  settings,
  icon,
  onChange,
  error,
}: {
  project: Project;
  settings: ProjectSettings;
  icon: ProjectIconSetting;
  onChange: (icon: ProjectIconSetting) => void;
  error: string | null;
}) {
  const pick = (kind: ProjectIconSetting["kind"]) =>
    onChange(
      kind === "auto"
        ? { kind }
        : kind === "monogram"
          ? { kind, text: icon.kind === "monogram" ? icon.text : undefined }
          : kind === "emoji"
            ? { kind, emoji: icon.kind === "emoji" ? icon.emoji : "" }
            : { kind, path: icon.kind === "file" ? icon.path : (project.logo?.path ?? "") },
    );
  // The preview shows what the panel holds; a `file` logo shows once the daemon has read it.
  const preview = { ...project, settings: { ...settings, icon: icon.kind === "file" ? undefined : icon } };
  const found = project.logo && (icon.kind === "auto" || icon.kind === "file") ? project.logo.path : null;
  return (
    <div className="mt-2.5 flex items-start gap-3">
      <span className="grid size-11 shrink-0 place-items-center rounded-xl border border-line-2 bg-panel-2 inset-shadow-highlight">
        <ProjectIcon project={preview} className="size-7" />
      </span>
      <div className="min-w-0 flex-1 space-y-1.5">
        <div className="flex flex-wrap items-center gap-1.5">
          <Select label="How the icon is made" className="w-52 min-w-0" value={icon.kind} onChange={pick} options={ICON_KINDS} />
          {icon.kind === "monogram" && (
            <input
              aria-label="Monogram letters"
              placeholder="Letters (from the name)"
              value={icon.text ?? ""}
              maxLength={6}
              onChange={(e) => onChange({ kind: "monogram", text: e.target.value || undefined })}
              className={`${field} w-44 font-mono uppercase`}
              spellCheck={false}
            />
          )}
          {icon.kind === "emoji" && (
            <input aria-label="Emoji" placeholder="🦀" value={icon.emoji} onChange={(e) => onChange({ kind: "emoji", emoji: e.target.value })} className={`${field} w-20 text-center`} />
          )}
          {icon.kind === "file" && (
            <input
              aria-label="Picture path"
              placeholder="public/logo.svg"
              value={icon.path}
              onChange={(e) => onChange({ kind: "file", path: e.target.value })}
              className={`${field} min-w-0 flex-1 font-mono text-[12px]`}
              spellCheck={false}
            />
          )}
        </div>
        {error ? (
          <p role="alert" className="text-[12px] text-danger">
            {error[0].toUpperCase() + error.slice(1)}.
          </p>
        ) : (
          <p className="text-[11.5px] text-subtle">
            {found ? (
              <>
                Using <code className="font-mono text-muted">{found}</code> from the project.
              </>
            ) : icon.kind === "auto" ? (
              "No logo found in the project: it shows its monogram."
            ) : icon.kind === "file" ? (
              "Saved, the machine reads it from the project (SVG, PNG, ICO, GIF, WebP or JPEG)."
            ) : null}
          </p>
        )}
      </div>
    </div>
  );
}

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
  const [color, setColor] = useState<IdentityColor | undefined>(project.settings.color);
  const [icon, setIcon] = useState<ProjectIconSetting>(project.settings.icon ?? { kind: "auto" });
  const [autoSettle, setAutoSettle] = useState<AutoSettle>(project.settings.auto_settle);
  const [cleanup, setCleanup] = useState<Cleanup>(project.settings.cleanup ?? {});
  const [cleaned, setCleaned] = useState<CleanupReport | null>(null);
  const iconChecked = checkProjectIcon(icon);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const saved = useMemo(() => new Set(project.settings.scripts.map((s) => s.id)), [project.settings.scripts]);

  const cleanupDays = cleanup.worktree_after_days ?? null;
  const keep = cleanup.keep ?? "uncommitted_changes";
  const settings: ProjectSettings = {
    ...project.settings,
    auto_pull: autoPull,
    defaults,
    scripts,
    color,
    icon: "icon" in iconChecked && iconChecked.icon.kind !== "auto" ? iconChecked.icon : undefined,
    auto_settle: { after_days: autoSettle.after_days, on_land: autoSettle.on_land, keep_changes: autoSettle.keep_changes || undefined },
    // Off and kept as the default leaves no trace, as the daemon writes it.
    cleanup: cleanupDays === null && keep === "uncommitted_changes" ? undefined : { worktree_after_days: cleanupDays ?? undefined, keep: keep === "uncommitted_changes" ? undefined : keep },
  };
  const changed = name.trim() !== project.name || canon(settings) !== canon(project.settings);
  // Closing with something unsaved (Escape, the panel's ×) is refused once and says why, so a
  // change is never dropped without a word; Discard or Save, then close.
  const [held, setHeld] = useState(false);
  useEscapeLayer(changed, () => setHeld(true));
  // So is opening something else: a card on the canvas, a row, ⌘K (it threw the edits away).
  useLeaveGuard(changed && !onDone, () => setHeld(true));
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

  const discard = () => {
    setName(project.name);
    setAutoPull(project.settings.auto_pull);
    setDefaults(project.settings.defaults);
    setScripts(project.settings.scripts);
    setColor(project.settings.color);
    setIcon(project.settings.icon ?? { kind: "auto" });
    setError(null);
  };

  const patchScript = (id: string, patch: Partial<ProjectScript>) => setScripts((all) => all.map((s) => (s.id === id ? { ...s, ...patch } : s)));

  return (
    <form
      className="text-[13px]"
      onSubmit={(e) => {
        e.preventDefault();
        if (changed && scriptsOk && online) void save();
      }}
    >
      <SettingsSection title="Name and icon" ids={[P.name.id, P.color.id, P.icon.id]}>
        <SettingRow
          s={P.name}
          control={<input id={`project-name-${project.id}`} aria-label="Name" value={name} onChange={(e) => setName(e.target.value)} className={`${field} w-56`} spellCheck={false} />}
        />
        <SettingRow s={P.color}>
          <div className="mt-2.5">
            <ColorPicker value={color} auto={projectColor({ ...project, settings: { ...project.settings, color: undefined } })} onChange={setColor} />
          </div>
        </SettingRow>
        <SettingRow s={P.icon}>
          <IconEditor project={project} settings={settings} icon={icon} onChange={setIcon} error={"error" in iconChecked ? iconChecked.error : null} />
        </SettingRow>
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

      <SettingsSection title="Settling and cleanup" ids={[P.autoSettle.id, P.onLand.id, P.keepChanges.id, P.cleanup.id, P.cleanupKeep.id]}>
        <SettingRow
          s={P.autoSettle}
          control={
            <Select
              label="Settle inactive threads"
              className="w-36"
              value={autoSettle.after_days === null ? "never" : String(autoSettle.after_days)}
              onChange={(v) => setAutoSettle((a) => ({ ...a, after_days: v === "never" ? null : Number(v) }))}
              options={dayOptions(SETTLE_DAYS, autoSettle.after_days)}
            />
          }
        />
        <SettingRow s={P.onLand} control={<Switch checked={autoSettle.on_land} onCheckedChange={(on) => setAutoSettle((a) => ({ ...a, on_land: on }))} aria-label={P.onLand.title} />} />
        <SettingRow s={P.keepChanges} control={<Switch checked={!!autoSettle.keep_changes} onCheckedChange={(on) => setAutoSettle((a) => ({ ...a, keep_changes: on }))} aria-label={P.keepChanges.title} />} />
        <SettingRow
          s={P.cleanup}
          control={
            <>
              <button
                type="button"
                disabled={!online || busy !== null}
                title="Remove the worktrees of this project's settled and archived threads now, with the same checks"
                onClick={() =>
                  act("cleanup", async () => setCleaned(await client.run<{ type: "cleaned" } & CleanupReport>(machine!.id, { type: "project.cleanup", project: project.id }, "cleaned")))
                }
                className={`${btn} shrink-0 whitespace-nowrap`}
              >
                <Eraser className="size-3.5" />
                {busy === "cleanup" ? <span className="shimmer">Cleaning up…</span> : "Clean up now"}
              </button>
              <Select
                label="Remove settled worktrees"
                className="w-36"
                value={cleanupDays === null ? "never" : String(cleanupDays)}
                onChange={(v) => setCleanup((c) => ({ ...c, worktree_after_days: v === "never" ? null : Number(v) }))}
                options={dayOptions(CLEANUP_DAYS, cleanupDays)}
              />
            </>
          }
        >
          {cleaned && (
            <p className="mt-2 text-[12px] text-muted" role="status">
              {cleanupText(cleaned)}
            </p>
          )}
        </SettingRow>
        <SettingRow
          s={P.cleanupKeep}
          control={
            <Select
              label="Keep a worktree that has"
              className="w-44"
              value={keep}
              onChange={(v) => setCleanup((c) => ({ ...c, keep: v }))}
              options={[
                { value: "uncommitted_changes", label: "Uncommitted changes" },
                { value: "any_local_files", label: "Any local files" },
              ]}
            />
          }
        />
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

      {/* While something's changed the bar stays in view at the bottom and says so, so a change
          is never left unsaved below the fold. */}
      <div
        role={changed ? "region" : undefined}
        aria-label={changed ? "Unsaved changes" : undefined}
        className={`flex items-center gap-1.5 ${changed ? "sticky bottom-0 z-10 -mx-1 border-t border-line bg-sheet px-1 py-3" : ""}`}
      >
        {changed && (
          <span role={held ? "alert" : undefined} className={`mr-auto text-[12.5px] ${held ? "text-fg" : "text-muted"}`}>
            {held ? "Save or discard your changes first" : "Unsaved changes"}
          </span>
        )}
        {changed && !onDone && (
          <button type="button" onClick={discard} className={btn}>
            Discard
          </button>
        )}
        <button type="submit" disabled={!changed || !scriptsOk || !online || busy !== null || !name.trim() || "error" in iconChecked} className={primary}>
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
