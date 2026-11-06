"use client";

/**
 * The owner's agents settings in the side panel (`{ kind: "settings", target: "owner" }`,
 * `#agents/settings/owner`, ⌘,): what follows the user to every device, T3 Code's client settings
 * that roll has a use for (`SettingsPanels.tsx` there: General, Chat, Threads, Keybindings), the
 * way to each machine's own settings, the owner's endpoint profiles (`endpoint-settings.tsx`), the
 * owner's model gateway (`gateway/section.tsx`), the
 * owner's Slack, Discord and GitHub (`integrations.tsx`),
 * and its Linear workspace (`linear-settings.tsx`). Saved by roll per owner
 * (`AgentsServices.settings`), so other devices change too; each change saves at once, as in T3.
 */

import { ChevronRight, SlidersHorizontal } from "lucide-react";
import { useMemo, useState, useSyncExternalStore } from "react";
import { reachable, type Machine, type ProviderInfo } from "@/lib/agents-api";
import type { AgentsServices } from "@/lib/agents-services";
import { setAgentsView } from "@/lib/agents-surface";
import { desktop } from "@/lib/desktop";
import { DEFAULT_OWNER_SETTINGS, RESTORABLE, type NotifyMode, type OwnerSettings } from "@/lib/owner-settings";
import { INTEGRATION_SETTINGS, IntegrationsSection } from "./integrations";
import { LINEAR_SETTINGS, LinearSettingsSection } from "./linear-settings";
import { ShareLinks } from "./share";
import { ENDPOINT_SETTINGS, EndpointProfilesSection } from "./endpoint-settings";
import { GATEWAY_SETTINGS, GatewaySection } from "./gateway/section";
import { DefaultsRows } from "./machine-settings";
import { SaveState, SettingRow, SettingsPage, SettingsSection, useSearching, type Setting, type SettingsElsewhere } from "./settings-ui";
import { MachineIcon } from "./identity";
import { dollars } from "./sandbox";
import { Segmented, Select, Switch } from "./ui";
import { useAgents } from "./use-agents";
import { useEscapeLayer } from "./escape-layers";

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
    description: "What a message does to a busy thread. ⌘/Ctrl+Enter (on a phone, holding Send) does the other one once.",
    terms: ["queue", "steer", "follow up", "interrupt", "running"],
  },
  model: { id: "new.model", title: "Model", section: "New threads", description: "On every machine, after the project's and the machine's defaults.", terms: ["provider", "default model", "effort"] },
  permissions: { id: "new.permissions", title: "Permissions", section: "New threads", terms: ["runtime mode", "full access", "auto edit", "ask", "approvals"] },
  mode: { id: "new.mode", title: "Plan first", section: "New threads", terms: ["interaction mode", "plan mode"] },
  workspace: { id: "new.workspace", title: "Workspace", section: "New threads", terms: ["worktree", "checkout"] },
  shares: { id: "shares", title: "Shared links", section: "Shared links", description: "Read-only replays of conversations, frozen when made. Revoking one removes it at once.", terms: ["share", "links", "public", "replay", "revoke"] },
  keyboard: { id: "keyboard", title: "Keyboard shortcuts", section: "Keyboard", terms: ["keys", "hotkeys", "keybindings", "shortcut"] },
  restore: { id: "restore", title: "Restore defaults", section: "General", description: "Put every setting on this page back (favourite models stay).", terms: ["reset"] },
} satisfies Record<string, Setting>;

const NOTIFY: { value: NotifyMode; label: string }[] = [
  { value: "off", label: "Off" },
  { value: "notify", label: "Notifications" },
  { value: "sound", label: "Sound" },
  { value: "both", label: "Notifications and sound" },
];

/** Every key the agents surface answers (each is handled where its row says; a test checks a few). */
const KEYS: [string, string][] = [
  ["⌘J", "Open or close the agents"],
  ["⌘K", "Search threads, projects and commands"],
  ["⌘N  ⌘⇧O", "A new agent"],
  ["⌘B", "Show or hide the threads"],
  ["⌘,", "These settings"],
  ["Esc", "Back to the canvas, then close"],
  ["A", "Automations"],
  ["F", "Focus one project"],
  ["⇧S", "The shelf: settled, snoozed and archived threads"],
  ["⌘⇧S", "Settle the open thread and go to the next"],
  ["⌘Z", "Undo the last settle, snooze or archive"],
  ["/", "Filter threads (in settings: search them)"],
  ["[  ]", "The canvas view before or after"],
  ["⌥1 – ⌥9", "A canvas view by its place"],
  ["⌥S", "Save the filter as a canvas view"],
  ["Enter", "Send (Shift+Enter: a new line), or open the focused card"],
  ["⌘Enter", "Steer instead of queueing, or the other way round, once"],
  ["Shift+Tab", "Plan first, or not"],
  ["← → ↑ ↓", "Pan the canvas"],
  ["+ − 0 1", "Zoom the canvas, fit it, actual size"],
  ["⇧D", "Lay the canvas out left to right, or top-down"],
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

/** Every row of the owner's settings, for its search and the other pages' (`elsewhere`). */
export function ownerSettingItems(machines: readonly Machine[], services: AgentsServices | null | undefined): Setting[] {
  return [
    S.notify,
    S.confirmDelete,
    S.send,
    S.followUp,
    S.model,
    S.permissions,
    S.mode,
    S.workspace,
    S.shares,
    ...machines.map((m) => ({ id: `machine.${m.id}`, title: m.name, section: "Machines", terms: ["machine settings", "providers", m.info?.os ?? "", m.sandbox ? "sandbox" : "computer"] })),
    ...(services ? [ENDPOINT_SETTINGS.owner] : []),
    ...(services?.gateway ? Object.values(GATEWAY_SETTINGS) : []),
    ...(services ? Object.values(INTEGRATION_SETTINGS) : []),
    ...(services ? LINEAR_SETTINGS : []),
    S.keyboard,
    S.restore,
  ];
}

export function OwnerSettingsPanel({ onClose, elsewhere }: { onClose: () => void; elsewhere?: readonly SettingsElsewhere[] }) {
  const { client, owner, prefs, services, demo, sandboxes } = useAgents();
  // What the sandboxes ran this month, deleted ones too (each card shows its own).
  const spend = sandboxes?.usage.size ? [...sandboxes.usage.values()].reduce((t, u) => ({ hours: t.hours + u.running_hours, usd: t.usd + u.price_usd }), { hours: 0, usd: 0 }) : null;
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
  const items = useMemo(() => ownerSettingItems(machines, services), [machines, services]);

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
    <SettingsPage icon={<SlidersHorizontal className="size-4 text-muted" />} title="Agents settings" subtitle={`${owner} · on every device`} onClose={onClose} items={items} elsewhere={elsewhere}>
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
        <DefaultsRows inherits="provider" rows={S} defaults={settings.defaults} providers={providers} disabled={disabled} onChange={(defaults) => void save({ defaults })} />
      </SettingsSection>
      <SettingsSection
        title="Machines"
        ids={machines.map((m) => `machine.${m.id}`)}
        action={
          spend && (
            <span className="font-mono text-[11.5px] text-subtle" title="Sandboxes' running time and cost this month (UTC), deleted ones included">
              Sandboxes this month: {spend.hours.toFixed(1)} h · {dollars(spend.usd)}
            </span>
          )
        }
      >
        {machines.map((m) => (
          <SettingRow
            key={m.id}
            s={{ id: `machine.${m.id}`, title: m.name, section: "Machines", description: m.info ? `${m.info.providers.map((p) => p.name).join(", ") || "No providers"}` : "Never connected" }}
            note={<MachineIcon machine={m} className="size-3" />}
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
      {services && (
        <SettingsSection title="Endpoints" ids={[ENDPOINT_SETTINGS.owner.id]}>
          <EndpointProfilesSection />
        </SettingsSection>
      )}
      {services?.gateway && <GatewaySection />}
      {services && <IntegrationsSection owner={owner} api={services.integrations} />}
      <LinearSettingsSection />
      {!demo && (
        <SettingsSection title="Shared links" ids={[S.shares.id]}>
          <SettingRow s={S.shares}>
            <ShareLinks owner={owner} flush />
          </SettingRow>
        </SettingsSection>
      )}
      <SettingsSection title="Keyboard" ids={[S.keyboard.id]}>
        <SettingRow s={S.keyboard}>
          <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-[12.5px]">
            {KEYS.map(([k, what]) => (
              <div key={k} className="contents">
                <dt>
                  <kbd className="rounded border border-line-2 bg-panel-2 px-1.5 py-0.5 font-mono text-[11px] text-muted">{k}</kbd>
                </dt>
                <dd className="text-muted">{what}</dd>
              </div>
            ))}
          </dl>
        </SettingRow>
      </SettingsSection>
      <Restore
        disabled={disabled || !RESTORABLE.some(changed)}
        onRestore={() => save(Object.fromEntries(RESTORABLE.map((k) => [k, DEFAULT_OWNER_SETTINGS[k]])) as Partial<OwnerSettings>)}
      />
    </SettingsPage>
  );
}

/** T3's "Restore defaults", asked once more. */
function Restore({ disabled, onRestore }: { disabled: boolean; onRestore: () => Promise<void> }) {
  const [asking, setAsking] = useState(false);
  useEscapeLayer(asking, () => setAsking(false));
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
