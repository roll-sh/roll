"use client";

/**
 * The owner's agents settings in the side panel (`{ kind: "settings", target: "owner" }`,
 * `#agents/settings/owner`, ⌘,): what follows the user to every device, T3 Code's client settings
 * that roll has a use for (`SettingsPanels.tsx` there: General, Chat, Threads, Keybindings), and
 * the way to each machine's own settings. Saved by roll per owner (`AgentsServices.settings`), so
 * other devices change too; each change saves at once, as in T3.
 */

import { ChevronRight, SlidersHorizontal } from "lucide-react";
import { useMemo, useState, useSyncExternalStore } from "react";
import { reachable, type ProviderInfo } from "@/lib/agents-api";
import { setAgentsView } from "@/lib/agents-surface";
import { desktop } from "@/lib/desktop";
import { DEFAULT_OWNER_SETTINGS, type NotifyMode, type OwnerSettings } from "@/lib/owner-settings";
import { DefaultsRows } from "./machine-settings";
import { SaveState, SettingRow, SettingsPage, SettingsSection, useSearching, type Setting } from "./settings-ui";
import { MachineIcon } from "./shared";
import { Segmented, Select, Switch } from "./ui";
import { useAgents } from "./use-agents";

const S = {
  notify: {
    id: "general.notifications",
    title: "Notifications",
    section: "General",
    description: "When an agent needs you or finishes while roll isn't in front.",
    terms: ["notify", "sound", "alert", "chime", "approval", "question", "done"],
  },
  confirmDelete: { id: "general.confirm-delete", title: "Confirm before deleting a thread", section: "General", description: "Ask once more before a thread and its worktree go.", terms: ["delete", "ask", "confirmation"] },
  send: { id: "chat.send", title: "Send with", section: "Chat", description: "The key that sends a message; the other breaks the line.", terms: ["enter", "shortcut", "submit", "keyboard", "cmd enter", "ctrl enter"] },
  followUp: {
    id: "chat.follow-up",
    title: "While a run is going",
    section: "Chat",
    description: "What a message does to a busy thread. ⌘/Ctrl+Enter does the other one once.",
    terms: ["queue", "steer", "follow up", "interrupt", "running"],
  },
  model: { id: "new.model", title: "Model", section: "New threads", description: "On every machine, after the project's and the machine's defaults.", terms: ["provider", "default model", "effort"] },
  permissions: { id: "new.permissions", title: "Permissions", section: "New threads", terms: ["runtime mode", "full access", "auto edit", "ask", "approvals"] },
  mode: { id: "new.mode", title: "Plan first", section: "New threads", terms: ["interaction mode", "plan mode"] },
  workspace: { id: "new.workspace", title: "Workspace", section: "New threads", terms: ["worktree", "checkout"] },
  keyboard: { id: "keyboard", title: "Keyboard shortcuts", section: "Keyboard", terms: ["keys", "hotkeys", "keybindings", "shortcut"] },
  restore: { id: "restore", title: "Restore defaults", section: "General", description: "Put every setting on this page back.", terms: ["reset"] },
} satisfies Record<string, Setting>;

const NOTIFY: { value: NotifyMode; label: string }[] = [
  { value: "off", label: "Off" },
  { value: "notify", label: "Notifications" },
  { value: "sound", label: "Sound" },
  { value: "both", label: "Notifications and sound" },
];

const KEYS: [string, string][] = [
  ["⌘J", "Open or close the agents"],
  ["⌘,", "These settings"],
  ["Esc", "Back to the canvas, then close"],
  ["/", "Search settings"],
  ["Enter", "Send (Shift+Enter: a new line)"],
  ["⌘Enter", "Steer instead of queueing, or the other way round, once"],
  ["Shift+Tab", "Plan first, or not"],
  ["+ − 0 1", "Zoom the canvas, fit it, actual size"],
  ["⌥↑ ⌥↓", "Move a card among its siblings"],
];

/** The browser's word on notifications, live. */
function usePermission(): NotificationPermission | "unsupported" {
  return useSyncExternalStore(
    (f) => {
      const t = setInterval(f, 1000);
      return () => clearInterval(t);
    },
    () => (typeof Notification === "undefined" ? "unsupported" : Notification.permission),
    () => "default",
  );
}

export function OwnerSettingsPanel({ onClose }: { onClose: () => void }) {
  const { client, owner, prefs } = useAgents();
  const settings = prefs?.settings ?? DEFAULT_OWNER_SETTINGS;
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const machines = [...client.data.machines.values()].map((r) => r.value).sort((a, b) => a.name.localeCompare(b.name));
  const providers = useMemo(() => {
    // Every provider on the owner's machines, once (the first machine's listing of it).
    const byId = new Map<string, ProviderInfo>();
    for (const m of machines) for (const p of m.info?.providers ?? []) if (!byId.has(p.id)) byId.set(p.id, p);
    return [...byId.values()];
  }, [machines]);
  const items = useMemo<Setting[]>(
    () => [
      S.notify,
      S.confirmDelete,
      S.send,
      S.followUp,
      S.model,
      S.permissions,
      S.mode,
      S.workspace,
      ...machines.map((m) => ({ id: `machine.${m.id}`, title: m.name, section: "Machines", terms: ["machine settings", "providers", m.info?.os ?? "", m.sandbox ? "sandbox" : "computer"] })),
      S.keyboard,
      S.restore,
    ],
    [machines],
  );

  const save = async (patch: Partial<OwnerSettings>) => {
    if (!prefs) return;
    setSaving(true);
    setError(null);
    try {
      await prefs.save(patch);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };
  const changed = (k: keyof OwnerSettings) => JSON.stringify(settings[k]) !== JSON.stringify(DEFAULT_OWNER_SETTINGS[k]);
  const reset = (k: keyof OwnerSettings) => (changed(k) && prefs ? () => save({ [k]: DEFAULT_OWNER_SETTINGS[k] }) : null);
  const disabled = !prefs?.loaded;

  return (
    <SettingsPage icon={<SlidersHorizontal className="size-4 text-muted" />} title="Agents settings" subtitle={`${owner} · on every device`} onClose={onClose} items={items}>
      <SaveState saving={saving} error={error ?? prefs?.error ?? null} offline={!prefs ? "Sign in to keep settings." : null} />
      <SettingsSection title="General" ids={[S.notify.id, S.confirmDelete.id]}>
        <SettingRow
          s={S.notify}
          onReset={reset("notifications")}
          control={<Select label="Notifications" className="w-52" disabled={disabled} value={settings.notifications} onChange={(v) => save({ notifications: v })} options={NOTIFY} />}
        >
          <NotifyPermission mode={settings.notifications} />
        </SettingRow>
        <SettingRow
          s={S.confirmDelete}
          onReset={reset("confirm_delete")}
          control={<Switch checked={settings.confirm_delete} disabled={disabled} onCheckedChange={(on) => save({ confirm_delete: on })} aria-label="Confirm before deleting a thread" />}
        />
      </SettingsSection>
      <SettingsSection title="Chat" ids={[S.send.id, S.followUp.id]}>
        <SettingRow
          s={S.send}
          onReset={reset("send")}
          control={
            <Segmented
              label="Send with"
              value={settings.send}
              onChange={(v) => save({ send: v })}
              options={[
                { value: "enter", label: "Enter" },
                { value: "mod_enter", label: "⌘/Ctrl+Enter" },
              ]}
            />
          }
        />
        <SettingRow
          s={S.followUp}
          onReset={reset("follow_up")}
          control={
            <Segmented
              label="While a run is going"
              value={settings.follow_up}
              onChange={(v) => save({ follow_up: v })}
              options={[
                { value: "queue", label: "Queue" },
                { value: "steer", label: "Steer" },
              ]}
            />
          }
        />
      </SettingsSection>
      <SettingsSection title="New threads" ids={[S.model.id, S.permissions.id, S.mode.id, S.workspace.id]}>
        <DefaultsRows rows={S} defaults={settings.defaults} providers={providers} disabled={disabled} onChange={(defaults) => void save({ defaults })} />
      </SettingsSection>
      <SettingsSection title="Machines" ids={machines.map((m) => `machine.${m.id}`)}>
        {machines.map((m) => (
          <SettingRow
            key={m.id}
            s={{ id: `machine.${m.id}`, title: m.name, section: "Machines", description: m.info ? `${m.info.providers.map((p) => p.name).join(", ") || "No providers"}` : "Never connected" }}
            note={<MachineIcon machine={m} className="size-3 text-subtle" />}
            control={
              <button
                type="button"
                onClick={() => setAgentsView({ kind: "settings", target: `m:${m.id}` })}
                aria-label={`${m.name} settings`}
                className="flex h-7 items-center gap-1 rounded-md px-2 text-[12px] text-muted hover:bg-fg/[0.06] hover:text-fg"
              >
                {reachable(m) ? "Settings" : "Offline"} <ChevronRight className="size-3.5" />
              </button>
            }
          />
        ))}
      </SettingsSection>
      <SettingsSection title="Keyboard" ids={[S.keyboard.id]}>
        <SettingRow s={S.keyboard}>
          <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-[12.5px]">
            {KEYS.map(([k, what]) => (
              <div key={k} className="contents">
                <dt>
                  <kbd className="rounded border border-line-2 bg-bg px-1.5 py-0.5 font-mono text-[11px] text-muted">{k}</kbd>
                </dt>
                <dd className="text-muted">{what}</dd>
              </div>
            ))}
          </dl>
        </SettingRow>
      </SettingsSection>
      <Restore disabled={disabled || !Object.keys(DEFAULT_OWNER_SETTINGS).some((k) => changed(k as keyof OwnerSettings))} onRestore={() => save(DEFAULT_OWNER_SETTINGS)} />
    </SettingsPage>
  );
}

/** T3's "Restore defaults", asked once more. */
function Restore({ disabled, onRestore }: { disabled: boolean; onRestore: () => Promise<void> }) {
  const [asking, setAsking] = useState(false);
  if (useSearching()) return null;
  return (
    <div className="flex items-center gap-2 px-0.5">
      {asking ? (
        <>
          <span className="text-[12px] text-muted">Put every setting here back to its default?</span>
          <button type="button" onClick={() => void onRestore().then(() => setAsking(false))} className="h-7 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg hover:opacity-85">
            Restore
          </button>
          <button type="button" onClick={() => setAsking(false)} className="h-7 rounded-md px-2 text-[12px] text-muted hover:text-fg">
            Cancel
          </button>
        </>
      ) : (
        <button type="button" disabled={disabled} onClick={() => setAsking(true)} className="h-7 rounded-md border border-line-2 px-2.5 text-[12px] text-muted hover:text-fg disabled:opacity-40">
          Restore defaults
        </button>
      )}
    </div>
  );
}

function NotifyPermission({ mode }: { mode: NotifyMode }) {
  const permission = usePermission();
  if (mode !== "notify" && mode !== "both") return null;
  if (desktop()) return <p className="mt-1.5 text-[12px] text-subtle">The desktop app notifies on its own.</p>;
  if (permission === "granted") return null;
  if (permission === "unsupported") return <p className="mt-1.5 text-[12px] text-subtle">This browser can't show notifications.</p>;
  if (permission === "denied") return <p className="mt-1.5 text-[12px] text-subtle">The browser blocks notifications from roll: allow them in its site settings.</p>;
  return (
    <p className="mt-1.5 flex items-center gap-2 text-[12px] text-subtle">
      The browser asks first.
      <button type="button" onClick={() => void Notification.requestPermission()} className="h-6 rounded-md border border-line-2 px-2 text-[12px] text-fg hover:bg-fg/[0.06]">
        Allow notifications
      </button>
    </p>
  );
}
