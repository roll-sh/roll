"use client";

/**
 * A machine's settings in the side panel (`{ kind: "settings", target: "m:<id>" }`), T3 Code's
 * settings for what lives on a machine (`SettingsPanels.tsx`, `ProviderSettingsForm.tsx`,
 * `ProviderModelsSection.tsx` there): its name, its sandbox, its providers (on or off, logins,
 * binary, arguments, environment, custom models), what new threads start with, worktrees, the git
 * identity commits are published with, projects and delegation. Every change sends
 * `machine.configure` with the settings whole; what the daemon says back (its `hello`) is the
 * truth. Credentials never go through `machine.configure`: a key is set with an endpoint, sealed
 * to the machine in the browser (`endpoint-settings.tsx`), or stays in the machine's `config.toml`.
 */

import { ChevronRight, Copy, KeyRound, Lock, LogIn, Pencil, Plus, RefreshCw, Trash2, X } from "lucide-react";
import { useMemo, useState } from "react";
import {
  type CustomModel,
  IDLE_COMPACTION_LABELS,
  type IdleCompaction,
  type Machine,
  type MachineConfig,
  type MachineSettings,
  machineState,
  type ProviderBase,
  type ProviderInfo,
  providerSettings,
  reachable,
  type RuntimeMode,
  type ThreadDefaults,
} from "@/lib/agents-api";
import { customModelError, envNameError, parseAutoCompact, settingsError, withFields, withProvider } from "@/lib/machine-settings";
import { setAgentsView } from "@/lib/agents-surface";
import { ENDPOINT_SETTINGS, MachineEndpoints, ProviderEndpointRow, providerEndpointSetting, type EndpointEdit } from "./endpoint-settings";
import { GATEWAY_MACHINE_SETTING, GatewayMachineCard, ProviderGatewayNote } from "./gateway/machine";
import { AddAgent, RegistryRemoveRow } from "./add-agent";
import { endpointOf } from "@/lib/agents-endpoints";
import { ModelPicker } from "./model-picker";
import { CustomModelEditor } from "./custom-model-editor";
import { ProviderLogo } from "./provider-logo";
import { UsageLimitBars } from "./usage-limits";
import { SandboxPanel } from "./sandbox";
import { field, Note, SaveState, SettingRow, SettingsPage, SettingsSection, TextField, useSearching, useShown, type Setting, type SettingsElsewhere } from "./settings-ui";
import { loginCommand } from "./shared";
import { ColorPicker, KindPicker, MachineIcon } from "./identity";
import { machineColor, machineKind } from "@/lib/agents-identity";
import { cn, Segmented, Select, Switch } from "./ui";
import { useAgents } from "./use-agents";
import { useEscapeLayer } from "./escape-layers";

const OS_NAMES: Record<string, string> = { macos: "macOS", linux: "Linux", windows: "Windows" };

/** Where each provider keeps its own state, the variable people set most (T3's `homePath`). */
const HOME_VAR: Record<string, string> = { codex: "CODEX_HOME", claude: "CLAUDE_CONFIG_DIR" };

// ── the rows, once: what the search finds them by ──

const S = {
  name: { id: "machine.name", title: "Name", section: "Machine", description: "How the machine is called on the canvas and in roll.", terms: ["rename"] },
  color: {
    id: "machine.color",
    title: "Colour",
    section: "Machine",
    description: "Its card, edges and icon carry it everywhere. Auto is made from the machine, the same on every device.",
    terms: ["color", "colour", "tint", "identity"],
  },
  kind: { id: "machine.kind", title: "Kind", section: "Machine", description: "Its icon. Auto: a sandbox is cloud, a Mac a laptop, Windows a desktop, the rest servers.", terms: ["icon", "laptop", "desktop", "server", "cloud", "container", "identity"] },
  system: { id: "machine.system", title: "System", section: "Machine", terms: ["status", "online", "os", "version", "roll agent", "daemon", "config.toml"] },
  remove: {
    id: "machine.remove",
    title: "Remove this machine",
    section: "Machine",
    description: "Revokes its token: the daemon stops connecting. What's on the machine stays there.",
    terms: ["delete", "revoke", "logout", "disconnect", "forget"],
  },
  sandbox: { id: "sandbox", title: "Sandbox", section: "Sandbox", description: "A machine roll runs for you, billed per running hour.", terms: ["start", "stop", "delete", "size", "price", "billing", "sleep", "wake", "usage"] },
  credentials: {
    id: "providers.credentials",
    title: "API keys and base URLs",
    section: "Providers",
    description: "roll never relays a credential in the clear. Add an endpoint under Endpoints below (its key is sealed to this machine in your browser), or set keys and base URLs in the daemon's config.toml on the machine.",
    terms: ["key", "token", "secret", "credential", "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "base url", "config.toml", "proxy"],
  },
  provider: (p: ProviderBase): Setting => ({ id: `p.${p.id}`, title: p.name, section: "Providers", description: "Offered for new threads, delegation and switching.", terms: ["enable", "disable", "turn off", "turn on", "login", "log in", p.id] }),
  binary: (p: ProviderBase): Setting => ({
    id: `p.${p.id}.binary`,
    title: "Binary",
    section: p.name,
    description: "The program roll starts for it. Empty: config.toml's, else the one on PATH.",
    terms: ["path", "executable", "command", "install", p.id],
  }),
  args: (p: ProviderBase): Setting => ({ id: `p.${p.id}.args`, title: "Extra arguments", section: p.name, description: "Passed after config.toml's, on its next session.", terms: ["flags", "launch", "cli", p.id] }),
  env: (p: ProviderBase): Setting => ({
    id: `p.${p.id}.env`,
    title: "Environment",
    section: p.name,
    description: "Variables for its next session, over config.toml's. Never a key or an address.",
    terms: ["variables", "env", ...(HOME_VAR[p.id] ? [HOME_VAR[p.id]] : []), p.id, ...p.env],
  }),
  models: (p: ProviderBase): Setting => ({ id: `p.${p.id}.models`, title: "Custom models", section: p.name, description: "Offered beside the models it lists, by slug.", terms: ["model", "slug", "add", p.id] }),
  autoCompact: (p: ProviderBase): Setting => ({
    id: `p.${p.id}.autocompact`,
    title: "Auto-compact after",
    section: p.name,
    description: "Compact after 100,000 to 1,000,000 tokens. Leave empty to use Claude's default.",
    terms: ["compact", "context", "window", "tokens", "autoCompactWindow", p.id],
  }),
  idleCompaction: (p: ProviderBase): Setting => ({
    id: `p.${p.id}.idle`,
    title: "Compact idle threads",
    section: p.name,
    description: "When to compact an idle thread over 200,000 tokens: after you return, just before its prompt cache expires, or never. Overrides the machine's default.",
    terms: ["compact", "idle", "cache", "return", p.id],
  }),
  idle: {
    id: "providers.idle",
    title: "Compact idle threads",
    section: "Providers",
    description: "When to compact an idle thread over 200,000 tokens, for providers that keep a prompt cache (Claude): when you come back to it, just before its cache expires, or never.",
    terms: ["compact", "idle", "cache", "return", "context"],
  },
  model: { id: "new.model", title: "Model", section: "New threads", description: "After the project's defaults; unset, your own defaults apply.", terms: ["provider", "default model", "effort"] },
  permissions: { id: "new.permissions", title: "Permissions", section: "New threads", terms: ["runtime mode", "full access", "auto edit", "ask", "approvals"] },
  mode: { id: "new.mode", title: "Plan first", section: "New threads", terms: ["interaction mode", "plan mode"] },
  workspace: { id: "new.workspace", title: "Workspace", section: "New threads", terms: ["worktree", "checkout"] },
  worktreeDir: { id: "worktrees.dir", title: "Worktree folder", section: "Worktrees", description: "Where new threads' worktrees go.", terms: ["directory", "location", "path"] },
  branchPrefix: { id: "worktrees.prefix", title: "Branch prefix", section: "Worktrees", description: "New threads' branches are <prefix>/<title>.", terms: ["branch name", "git"] },
  git: { id: "git.identity", title: "Commit as", section: "Git", description: "Who publishing commits as. Empty: the repository's git config, else roll.", terms: ["author", "name", "email", "identity", "user.name", "user.email", "publish"] },
  cloneDir: { id: "projects.clone", title: "Clone folder", section: "Projects", description: "Where roll code repos are cloned as projects.", terms: ["directory", "clone", "repos", "location"] },
  pullEvery: { id: "projects.pull", title: "Auto-pull checks", section: "Projects", description: "How often projects with auto-pull fetch their remote.", terms: ["pull", "fetch", "interval", "sync", "auto-pull"] },
  maxDepth: { id: "delegation.depth", title: "Subagent depth", section: "Delegation", description: "How deep subagents may delegate in turn.", terms: ["subagents", "nesting", "delegate", "orchestrator"] },
  maxChildren: { id: "delegation.children", title: "Tasks per thread", section: "Delegation", description: "Unfinished delegated tasks one thread may have.", terms: ["subagents", "parallel", "concurrent", "delegate"] },
} satisfies Record<string, Setting | ((p: ProviderBase) => Setting)>;

/** Claude's own knobs: T3's Claude instance config has "Auto-compact after" and idle compaction. */
const compacts = (p: ProviderBase) => p.id === "claude";
const providerRows = (p: ProviderBase) => [
  S.provider(p),
  providerEndpointSetting(p),
  S.binary(p),
  S.args(p),
  S.env(p),
  S.models(p),
  ...(compacts(p) ? [S.autoCompact(p), S.idleCompaction(p)] : []),
];

/** Every row of a machine's settings, for its search and the other pages' (`elsewhere`). */
export function machineSettingItems(config: MachineConfig | null, sandbox: boolean): Setting[] {
  const providers = config?.providers ?? [];
  return [
    S.name,
    S.color,
    S.kind,
    S.system,
    ...(sandbox ? [S.sandbox] : [S.remove]),
    S.credentials,
    S.idle,
    ...providers.flatMap(providerRows),
    ENDPOINT_SETTINGS.machine,
    GATEWAY_MACHINE_SETTING,
    S.model,
    S.permissions,
    S.mode,
    S.workspace,
    S.worktreeDir,
    S.branchPrefix,
    S.git,
    S.cloneDir,
    S.pullEvery,
    S.maxDepth,
    S.maxChildren,
  ];
}

/**
 * Its colour and kind: the relay keeps them with its name, so they change while it's offline or
 * asleep too, and every device shows them at once.
 */
function AppearanceRows({ machine, onError }: { machine: Machine; onError: (e: string | null) => void }) {
  const { services, owner } = useAgents();
  const style = async (s: Parameters<NonNullable<typeof services>["machines"]["style"]>[2]) => {
    if (!services) return;
    onError(null);
    await services.machines.style(owner, machine.id, s).catch((e: unknown) => onError(e instanceof Error ? e.message : String(e)));
  };
  const auto = { ...machine, appearance: {} };
  return (
    <>
      <SettingRow s={S.color}>
        <div className="mt-2.5">
          <ColorPicker value={machine.appearance.color} auto={machineColor(auto)} onChange={(c) => style({ color: c ?? null })} />
        </div>
      </SettingRow>
      <SettingRow s={S.kind}>
        <div className="mt-2.5">
          <KindPicker value={machine.appearance.kind} auto={machineKind(auto)} color={machineColor(machine)} onChange={(k) => style({ kind: k ?? null })} />
        </div>
      </SettingRow>
    </>
  );
}

const PULL_EVERY = [60, 300, 900, 3600, 0];
const pullLabel = (s: number) => (s === 0 ? "Never" : s < 3600 ? `Every ${s / 60} min` : `Every ${s / 3600} h`);
const RUNTIME: { value: RuntimeMode; label: string }[] = [
  { value: "full_access", label: "Full access" },
  { value: "auto_edit", label: "Auto edit" },
  { value: "ask", label: "Ask" },
];

export function MachineSettingsPanel({ machine, onClose, elsewhere }: { machine: Machine; onClose: () => void; elsewhere?: readonly SettingsElsewhere[] }) {
  const { client, services, owner } = useAgents();
  const config = machine.info?.settings ?? null;
  const set = useMemo(() => config?.set ?? {}, [config]);
  const online = reachable(machine);
  const editable = online && !!config;
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [endpointEdit, setEndpointEdit] = useState<EndpointEdit | null>(null);

  /** Send the settings whole; `false` when refused (the error says why). */
  const save = async (next: MachineSettings): Promise<boolean> => {
    const refused = settingsError(next);
    if (refused) {
      setError(refused);
      return false;
    }
    setSaving(true);
    setError(null);
    try {
      await client.run(machine.id, { type: "machine.configure", settings: next }, "ok");
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setSaving(false);
    }
  };

  const offline = !online
    ? `${machine.name} is ${machineState(machine)}: its settings change once it's back.`
    : !config
      ? machine.info
        ? `This machine's roll is too old to change its settings from here: update it and run roll agent up again.`
        : "Not connected yet."
      : null;
  const all = useMemo(() => machineSettingItems(config, !!machine.sandbox), [config, machine.sandbox]);
  const ctx: Ctx = { machine, config, set, editable, save, editEndpoint: setEndpointEdit };
  return (
    <SettingsPage icon={<MachineIcon machine={machine} tile className="size-5" />} title={machine.name} subtitle="Machine settings" onClose={onClose} items={all} elsewhere={elsewhere}>
      <SaveState saving={saving} error={error} offline={offline} />
      <SettingsSection title="Machine" ids={[S.name.id, S.color.id, S.kind.id, S.system.id, S.remove.id]}>
        <SettingRow
          s={S.name}
          control={
            <TextField
              label="Machine name"
              value={machine.name}
              disabled={!services}
              onSave={async (name) => {
                if (!name || !services) return;
                setError(null);
                await services.machines.rename(owner, machine.id, name).catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
              }}
            />
          }
        />
        <AppearanceRows machine={machine} onError={setError} />
        <SystemRow machine={machine} config={config} />
        {!machine.sandbox && <RemoveRow machine={machine} onError={setError} onRemoved={onClose} />}
      </SettingsSection>
      {machine.sandbox && (
        <SettingsSection title="Sandbox" ids={[S.sandbox.id]}>
          <SettingRow s={S.sandbox}>
            <div className="mt-2.5">
              <SandboxPanel machine={machine} />
            </div>
          </SettingRow>
        </SettingsSection>
      )}
      <Providers ctx={ctx} />
      <SettingsSection title="Endpoints" ids={[ENDPOINT_SETTINGS.machine.id]}>
        <MachineEndpoints machine={machine} editing={endpointEdit} onEdit={setEndpointEdit} />
      </SettingsSection>
      <GatewayMachineCard machine={machine} />
      <NewThreads ctx={ctx} />
      <SettingsSection title="Worktrees" ids={[S.worktreeDir.id, S.branchPrefix.id]}>
        <SettingRow
          s={S.worktreeDir}
          onReset={set.worktree_dir !== undefined && editable ? () => save(withFields(set, { worktree_dir: undefined })) : null}
          control={<TextField label="Worktree folder" mono value={set.worktree_dir ?? ""} placeholder={config?.base.worktree_dir} disabled={!editable} onSave={(v) => save(withFields(set, { worktree_dir: v }))} />}
        />
        <SettingRow
          s={S.branchPrefix}
          onReset={set.branch_prefix !== undefined && editable ? () => save(withFields(set, { branch_prefix: undefined })) : null}
          control={<TextField label="Branch prefix" mono className="w-40" value={set.branch_prefix ?? ""} placeholder={config?.base.branch_prefix ?? "roll"} disabled={!editable} onSave={(v) => save(withFields(set, { branch_prefix: v }))} />}
        />
      </SettingsSection>
      <SettingsSection title="Git" ids={[S.git.id]}>
        <GitIdentity ctx={ctx} />
      </SettingsSection>
      <SettingsSection title="Projects" ids={[S.cloneDir.id, S.pullEvery.id]}>
        <SettingRow
          s={S.cloneDir}
          onReset={set.clone_dir !== undefined && editable ? () => save(withFields(set, { clone_dir: undefined })) : null}
          control={<TextField label="Clone folder" mono value={set.clone_dir ?? ""} placeholder={config?.base.clone_dir ?? "~/roll"} disabled={!editable} onSave={(v) => save(withFields(set, { clone_dir: v }))} />}
        />
        <SettingRow
          s={S.pullEvery}
          onReset={set.pull_every !== undefined && editable ? () => save(withFields(set, { pull_every: undefined })) : null}
          control={
            <Select
              label="Auto-pull checks"
              className="w-44"
              disabled={!editable}
              value={String(set.pull_every ?? config?.base.pull_every ?? 60)}
              onChange={(v) => save(withFields(set, { pull_every: Number(v) }))}
              options={[...new Set([...PULL_EVERY, set.pull_every ?? config?.base.pull_every ?? 60])].map((s) => ({ value: String(s), label: pullLabel(s) }))}
            />
          }
        />
      </SettingsSection>
      <SettingsSection title="Delegation" ids={[S.maxDepth.id, S.maxChildren.id]}>
        <SettingRow
          s={S.maxDepth}
          onReset={set.max_depth !== undefined && editable ? () => save(withFields(set, { max_depth: undefined })) : null}
          control={
            <Select
              label="Subagent depth"
              className="w-36"
              disabled={!editable}
              value={String(set.max_depth ?? config?.base.max_depth ?? 3)}
              onChange={(v) => save(withFields(set, { max_depth: Number(v) }))}
              options={[0, 1, 2, 3, 4, 5, 6, 7, 8].map((n) => ({ value: String(n), label: n === 0 ? "No subagents" : `${n} deep` }))}
            />
          }
        />
        <SettingRow
          s={S.maxChildren}
          onReset={set.max_children !== undefined && editable ? () => save(withFields(set, { max_children: undefined })) : null}
          control={
            <Select
              label="Tasks per thread"
              className="w-36"
              disabled={!editable}
              value={String(set.max_children ?? config?.base.max_children ?? 6)}
              onChange={(v) => save(withFields(set, { max_children: Number(v) }))}
              options={[...new Set([1, 2, 3, 4, 6, 8, 12, 16, 24, 32, set.max_children ?? config?.base.max_children ?? 6])].sort((a, b) => a - b).map((n) => ({ value: String(n), label: `${n} at once` }))}
            />
          }
        />
      </SettingsSection>
      <OwnerLink />
    </SettingsPage>
  );
}

/** To the owner's settings (T3's sidebar has both on one page; here they're a panel apart). */
function OwnerLink() {
  if (useSearching()) return null;
  return (
    <button type="button" onClick={() => setAgentsView({ kind: "settings", target: "owner" })} className="flex w-full items-center gap-2 rounded-xl border border-line-2 bg-raised px-3.5 py-3 text-left text-[13px] text-muted shadow-card inset-shadow-highlight transition hover:border-line-3 hover:text-fg">
      <span className="flex-1">Your agents settings: notifications, the send key, defaults on every machine</span>
      <ChevronRight className="size-4" />
    </button>
  );
}

interface Ctx {
  machine: Machine;
  config: MachineConfig | null;
  set: MachineSettings;
  editable: boolean;
  save: (next: MachineSettings) => Promise<boolean>;
  /** Open the Endpoints section's editor (New endpoint… from a provider's card). */
  editEndpoint: (e: EndpointEdit | null) => void;
}

function SystemRow({ machine, config }: { machine: Machine; config: MachineConfig | null }) {
  const info = machine.info;
  const state = machineState(machine);
  return (
    <SettingRow
      s={{ ...S.system, description: info ? `${OS_NAMES[info.os] ?? info.os} · ${info.arch} · roll ${info.version}` : "Never connected" }}
      control={
        <span className="flex items-center gap-1.5 text-[12px] text-muted">
          <span className={cn("size-1.5 rounded-full", machine.online ? "bg-ok" : "bg-subtle")} aria-hidden />
          {state}
        </span>
      }
    >
      {config && <ConfigPath path={config.config_path} />}
    </SettingRow>
  );
}

function ConfigPath({ path }: { path: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <p className="mt-2 flex min-w-0 items-center gap-1.5 text-[11.5px] text-subtle">
      <span className="shrink-0">Its config file</span>
      <code className="min-w-0 truncate font-mono text-muted" title={path}>
        {path}
      </code>
      <button
        type="button"
        aria-label="Copy the config file's path"
        title={copied ? "Copied" : "Copy"}
        onClick={() => void navigator.clipboard?.writeText(path).then(() => setCopied(true))}
        className="grid size-5 shrink-0 place-items-center rounded hover:bg-fg/[0.06] hover:text-fg"
      >
        <Copy className="size-3" />
      </button>
    </p>
  );
}

function RemoveRow({ machine, onError, onRemoved }: { machine: Machine; onError: (e: string | null) => void; onRemoved: () => void }) {
  const { services, owner } = useAgents();
  const [asking, setAsking] = useState(false);
  useEscapeLayer(asking, () => setAsking(false));
  const [busy, setBusy] = useState(false);
  const remove = async () => {
    if (!services) return;
    setBusy(true);
    onError(null);
    try {
      await services.machines.remove(owner, machine.id);
      onRemoved();
    } catch (e) {
      onError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
      setAsking(false);
    }
  };
  return (
    <SettingRow
      s={S.remove}
      control={
        asking ? (
          <span className="flex items-center gap-1">
            <button type="button" disabled={busy} onClick={remove} className="h-7 rounded-md bg-danger/10 px-2.5 text-[12px] font-medium text-danger hover:bg-danger/20">
              {busy ? <span className="shimmer">Removing…</span> : `Remove ${machine.name}`}
            </button>
            <button type="button" onClick={() => setAsking(false)} className="grid size-7 place-items-center rounded-md text-subtle hover:text-fg" aria-label="Keep it">
              <X className="size-3.5" />
            </button>
          </span>
        ) : (
          <button type="button" disabled={!services} onClick={() => setAsking(true)} className="flex h-7 items-center gap-1.5 rounded-md border border-line-2 px-2.5 text-[12px] text-danger hover:bg-danger/10 disabled:opacity-50">
            <Trash2 className="size-3.5" /> Remove
          </button>
        )
      }
    />
  );
}

// ── providers ──

function Providers({ ctx }: { ctx: Ctx }) {
  const { client } = useAgents();
  const [checking, setChecking] = useState(false);
  const providers = ctx.config?.providers ?? (ctx.machine.info?.providers ?? []).map((p) => ({ id: p.id, name: p.name, binary: null, args: 0, env: [] }));
  const ids = [S.credentials.id, S.idle.id, ...providers.flatMap((p) => providerRows(p).map((r) => r.id))];
  const refresh = async () => {
    setChecking(true);
    try {
      await client.run(ctx.machine.id, { type: "provider.refresh" });
    } finally {
      setChecking(false);
    }
  };
  return (
    <SettingsSection
      title="Providers"
      ids={ids}
      action={
        <span className="flex items-center gap-0.5">
          <AddAgent machine={ctx.machine} />
          <button type="button" disabled={!reachable(ctx.machine) || checking} onClick={refresh} className="flex h-6 items-center gap-1 rounded-md px-1.5 text-[11.5px] text-subtle hover:bg-fg/[0.06] hover:text-fg disabled:opacity-50" title="Check providers again (after a login)">
            <RefreshCw className={cn("size-3", checking && "animate-spin motion-reduce:animate-none")} /> Check again
          </button>
        </span>
      }
    >
      {providers.map((p) => (
        <ProviderCard key={p.id} ctx={ctx} base={p} info={ctx.machine.info?.providers.find((i) => i.id === p.id)} />
      ))}
      <SettingRow
        s={S.idle}
        onReset={ctx.set.idle_compaction !== undefined && ctx.editable ? () => ctx.save(withFields(ctx.set, { idle_compaction: undefined })) : null}
        control={
          <Select<IdleCompaction>
            label="Compact idle threads"
            className="w-56"
            disabled={!ctx.editable}
            value={ctx.set.idle_compaction ?? "on_return"}
            onChange={(v) => void ctx.save(withFields(ctx.set, { idle_compaction: v }))}
            options={IDLE_CHOICES}
          />
        }
      />
      <SettingRow s={S.credentials} note={<KeyRound className="size-3 text-subtle" aria-hidden />}>
        {ctx.config && <ConfigPath path={ctx.config.config_path} />}
      </SettingRow>
    </SettingsSection>
  );
}

function providerStatus(on: boolean, info: ProviderInfo | undefined): string {
  if (!on) return "Off";
  if (!info) return "Not reported";
  if (!info.installed) return info.detail ?? "Not installed";
  const who = info.auth === "ready" ? "logged in" : info.auth === "missing" ? "needs a login" : (info.detail ?? "login unknown");
  return [info.version, who].filter(Boolean).join(" · ");
}

function ProviderCard({ ctx, base, info }: { ctx: Ctx; base: ProviderBase; info: ProviderInfo | undefined }) {
  const { client, focus } = useAgents();
  const p = providerSettings(ctx.set, base.id);
  const [open, setOpen] = useState(false);
  const searching = useSearching();
  const detailsShown = useShown(providerRows(base).slice(1).map((r) => r.id));
  const headShown = useShown([S.provider(base).id]);
  const expanded = searching ? detailsShown : open;
  const [busy, setBusy] = useState(false);
  if (!headShown && !detailsShown) return null;
  const login = async () => {
    if (!info) return;
    setBusy(true);
    try {
      const r = await client.run<{ type: "created"; id: string }>(ctx.machine.id, { type: "terminal.open", command: loginCommand(info, !!ctx.machine.sandbox), cols: 100, rows: 26 }, "created");
      focus(`x:${r.id}`);
    } finally {
      setBusy(false);
    }
  };
  const changed =
    p.binary !== undefined || p.args.length > 0 || Object.keys(p.env).length > 0 || p.models.length > 0 || p.auto_compact_window !== undefined || p.idle_compaction !== undefined;
  const via = endpointOf(ctx.machine.info, base.id);
  return (
    <div data-setting={S.provider(base).id} className="text-[13px]">
      <div className="flex items-center gap-3 px-3.5 py-3">
        <ProviderLogo provider={base} className="size-5" />
        <div className="min-w-0 flex-1">
          {/* The name keeps its line; the notes give way (on a phone "Claude Code" wrapped beside "via …"). */}
          <div className="flex min-w-0 items-center gap-1.5">
            <h4 className="shrink-0 font-medium tracking-[-0.01em] whitespace-nowrap">{base.name}</h4>
            {changed && <Note>customized</Note>}
            {via && (
              <Note title={`Its requests go to the endpoint ${via.name}`} className="min-w-0 truncate">
                via {via.name}
              </Note>
            )}
          </div>
          <p className="truncate text-[12px] text-subtle">{p.enabled && info?.checking ? <span className="shimmer">Checking…</span> : providerStatus(p.enabled, info)}</p>
        </div>
        {p.enabled && info?.installed && info.auth === "missing" && (
          <button type="button" disabled={!reachable(ctx.machine) || busy} onClick={login} className="flex h-7 items-center gap-1.5 rounded-md border border-line-2 px-2 text-[12px] hover:bg-fg/[0.06] disabled:opacity-50">
            <LogIn className="size-3.5" /> {busy ? <span className="shimmer">Opening…</span> : "Log in"}
          </button>
        )}
        <Switch checked={p.enabled} disabled={!ctx.editable} onCheckedChange={(on) => void ctx.save(withProvider(ctx.set, base.id, { enabled: on }))} aria-label={`Offer ${base.name}`} />
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          aria-expanded={expanded}
          aria-label={`${base.name} details`}
          className="grid size-7 place-items-center rounded-md text-subtle hover:bg-fg/[0.06] hover:text-fg"
        >
          <ChevronRight className={cn("size-4 transition-transform duration-200 motion-reduce:transition-none", expanded && "rotate-90")} />
        </button>
      </div>
      {p.enabled && info && <UsageLimitBars info={info} className="pb-3 pl-[50px] pr-[86px]" />}
      {p.enabled && via?.kind === "gateway" && (
        <div className="pb-3 pl-[50px] pr-3.5">
          <ProviderGatewayNote machine={ctx.machine} provider={base.id} />
        </div>
      )}
      {expanded && (
        <div className="mx-3.5 mb-3 divide-y divide-line overflow-hidden rounded-lg border border-line-2 bg-panel-2">
          <ProviderEndpointRow machine={ctx.machine} provider={base} onNew={() => ctx.editEndpoint({ provider: base.id })} />
          <SettingRow
            s={S.binary(base)}
            onReset={p.binary !== undefined && ctx.editable ? () => ctx.save(withProvider(ctx.set, base.id, { binary: undefined })) : null}
            control={
              <TextField
                label={`${base.name} binary`}
                mono
                value={p.binary ?? ""}
                placeholder={base.binary ?? `${base.id.replace(/^acp:/, "")} (on PATH)`}
                disabled={!ctx.editable}
                onSave={(v) => ctx.save(withProvider(ctx.set, base.id, { binary: v || undefined }))}
              />
            }
          />
          <SettingRow s={S.args(base)} note={base.args > 0 ? <Note title="config.toml gives it arguments too (not shown: they may hold a key)">+{base.args} in config.toml</Note> : undefined}>
            <ListEditor
              label={`${base.name} argument`}
              items={p.args}
              placeholder="--flag"
              disabled={!ctx.editable}
              onChange={(args) => ctx.save(withProvider(ctx.set, base.id, { args }))}
            />
          </SettingRow>
          <SettingRow s={S.env(base)}>
            <EnvEditor base={base} env={p.env} disabled={!ctx.editable} configPath={ctx.config?.config_path} onChange={(env) => ctx.save(withProvider(ctx.set, base.id, { env }))} />
          </SettingRow>
          <SettingRow s={S.models(base)}>
            <ModelsEditor base={base} info={info} models={p.models} disabled={!ctx.editable} onChange={(models) => ctx.save(withProvider(ctx.set, base.id, { models }))} />
          </SettingRow>
          {compacts(base) && <CompactionRows ctx={ctx} base={base} />}
          <RegistryRemoveRow machine={ctx.machine} base={base} />
        </div>
      )}
    </div>
  );
}

const IDLE_CHOICES = (["on_return", "before_cache_expires", "off"] as const).map((v) => ({ value: v, label: IDLE_COMPACTION_LABELS[v] }));
const USE_DEFAULT = "default";

/** Claude's compaction knobs (T3's Claude instance: "Auto-compact after", idle compaction with "Use default"). */
function CompactionRows({ ctx, base }: { ctx: Ctx; base: ProviderBase }) {
  const p = providerSettings(ctx.set, base.id);
  const [error, setError] = useState<string | null>(null);
  const machineDefault = IDLE_COMPACTION_LABELS[ctx.set.idle_compaction ?? "on_return"];
  return (
    <>
      <SettingRow
        s={S.autoCompact(base)}
        onReset={p.auto_compact_window !== undefined && ctx.editable ? () => ctx.save(withProvider(ctx.set, base.id, { auto_compact_window: undefined })) : null}
        control={
          <TextField
            label={`${base.name} auto-compact after`}
            mono
            className="w-36"
            value={p.auto_compact_window !== undefined ? String(p.auto_compact_window) : ""}
            placeholder="e.g. 300000"
            disabled={!ctx.editable}
            onSave={(v) => {
              const parsed = parseAutoCompact(v);
              if (!parsed.ok) return setError(parsed.error);
              setError(null);
              return ctx.save(withProvider(ctx.set, base.id, { auto_compact_window: parsed.value }));
            }}
          />
        }
      >
        {error && (
          <p role="alert" className="mt-1.5 text-[12px] text-danger">
            {error}
          </p>
        )}
      </SettingRow>
      <SettingRow
        s={S.idleCompaction(base)}
        control={
          <Select
            label={`${base.name}: compact idle threads`}
            className="w-56"
            disabled={!ctx.editable}
            value={p.idle_compaction ?? USE_DEFAULT}
            onChange={(v) => void ctx.save(withProvider(ctx.set, base.id, { idle_compaction: v === USE_DEFAULT ? undefined : (v as IdleCompaction) }))}
            options={[{ value: USE_DEFAULT, label: `Use default (${machineDefault})` }, ...IDLE_CHOICES]}
          />
        }
      />
    </>
  );
}

const chip = "inline-flex max-w-full items-center gap-1 rounded-md border border-line-2 bg-raised py-0.5 pr-0.5 pl-1.5 font-mono text-[11.5px]";
const smallBtn = "flex h-8 shrink-0 items-center gap-1 rounded-lg border border-line-2 bg-raised px-2.5 text-[12px] shadow-card transition-colors hover:bg-fg/[0.06] disabled:opacity-50";

function Remove({ label, onClick, disabled }: { label: string; onClick: () => void; disabled?: boolean }) {
  return (
    <button type="button" aria-label={label} title={label} disabled={disabled} onClick={onClick} className="grid size-5 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-danger disabled:opacity-40">
      <X className="size-3" />
    </button>
  );
}

/** Values in order, as chips; Enter adds one. */
function ListEditor({ label, items, placeholder, disabled, onChange }: { label: string; items: string[]; placeholder: string; disabled: boolean; onChange: (items: string[]) => Promise<boolean> }) {
  const [draft, setDraft] = useState("");
  const add = async () => {
    if (!draft.trim()) return;
    if (await onChange([...items, draft.trim()])) setDraft("");
  };
  return (
    <div className="mt-2 space-y-2">
      {items.length > 0 && (
        <ul className="flex flex-wrap gap-1" aria-label={`${label}s`}>
          {items.map((a, i) => (
            <li key={`${a}-${i}`} className={chip}>
              <span className="truncate">{a}</span>
              <Remove label={`Remove ${a}`} disabled={disabled} onClick={() => void onChange(items.filter((_, j) => j !== i))} />
            </li>
          ))}
        </ul>
      )}
      <form
        className="flex gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          void add();
        }}
      >
        <input aria-label={`New ${label}`} value={draft} onChange={(e) => setDraft(e.target.value)} placeholder={placeholder} disabled={disabled} spellCheck={false} className={cn(field, "flex-1 font-mono text-[12px]")} />
        <button type="submit" disabled={disabled || !draft.trim()} className={smallBtn}>
          <Plus className="size-3.5" /> Add
        </button>
      </form>
    </div>
  );
}

function EnvEditor({ base, env, disabled, configPath, onChange }: { base: ProviderBase; env: Record<string, string>; disabled: boolean; configPath?: string; onChange: (env: Record<string, string>) => Promise<boolean> }) {
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const add = async () => {
    const n = name.trim();
    const e = envNameError(n);
    if (e) return setProblem(configPath && e.includes("config.toml") ? `${e} (${configPath}).` : e);
    setProblem(null);
    if (await onChange({ ...env, [n]: value })) {
      setName("");
      setValue("");
    }
  };
  const rows = Object.entries(env);
  const fromFile = base.env.filter((n) => !(n in env));
  return (
    <div className="mt-2 space-y-2">
      {(rows.length > 0 || fromFile.length > 0) && (
        <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line-2 bg-raised" aria-label={`${base.name} environment`}>
          {fromFile.map((n) => (
            <li key={n} className="flex items-center gap-2 px-2.5 py-1.5 font-mono text-[12px]" title="Set in config.toml on the machine; its value never leaves it">
              <Lock className="size-3 shrink-0 text-subtle" aria-hidden />
              <span className="min-w-0 truncate">{n}</span>
              <span className="ml-auto shrink-0 font-sans text-[11.5px] text-subtle">set in config.toml</span>
            </li>
          ))}
          {rows.map(([n, v]) => (
            <li key={n} className="flex items-center gap-2 px-2.5 py-1.5 font-mono text-[12px]">
              <span className="shrink-0">{n}</span>
              <span className="text-subtle">=</span>
              <span className="min-w-0 flex-1 truncate text-muted" title={v}>
                {v || '""'}
              </span>
              <Remove label={`Remove ${n}`} disabled={disabled} onClick={() => void onChange(Object.fromEntries(rows.filter(([k]) => k !== n)))} />
            </li>
          ))}
        </ul>
      )}
      <form
        className="flex flex-wrap gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          void add();
        }}
      >
        <input
          aria-label="Variable name"
          value={name}
          onChange={(e) => {
            setName(e.target.value);
            setProblem(null);
          }}
          placeholder="NAME"
          disabled={disabled}
          spellCheck={false}
          className={cn(field, "w-44 font-mono text-[12px]")}
        />
        <input aria-label="Variable value" value={value} onChange={(e) => setValue(e.target.value)} placeholder="value" disabled={disabled} spellCheck={false} className={cn(field, "min-w-24 flex-1 font-mono text-[12px]")} />
        <button type="submit" disabled={disabled || !name.trim()} className={smallBtn}>
          <Plus className="size-3.5" /> Add
        </button>
      </form>
      {problem && (
        <p role="alert" className="text-[12px] text-danger">
          {problem}
        </p>
      )}
    </div>
  );
}

function ModelsEditor({
  base,
  info,
  models,
  disabled,
  onChange,
}: {
  base: ProviderBase;
  info: ProviderInfo | undefined;
  models: CustomModel[];
  disabled: boolean;
  onChange: (models: CustomModel[]) => Promise<boolean>;
}) {
  const [slug, setSlug] = useState("");
  const [name, setName] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  // The custom model whose name and options are open (T3's "Edit name and options").
  const [editing, setEditing] = useState<string | null>(null);
  const edited = models.find((m) => m.id === editing);
  const add = async () => {
    const e = customModelError(slug, info, models);
    if (e) return setProblem(e);
    setProblem(null);
    if (await onChange([...models, name.trim() ? { id: slug.trim(), name: name.trim() } : { id: slug.trim() }])) {
      setSlug("");
      setName("");
    }
  };
  return (
    <div className="mt-2 space-y-2">
      {models.length > 0 && (
        <ul className="flex flex-wrap gap-1" aria-label={`${base.name} custom models`}>
          {models.map((m) => (
            <li key={m.id} className={cn(chip, editing === m.id && "border-accent/50")}>
              <span className="truncate">{m.name ? `${m.name} · ${m.id}` : m.id}</span>
              {m.options?.length ? <span className="shrink-0 font-sans text-[10.5px] text-subtle">{m.options.length === 1 ? "1 option" : `${m.options.length} options`}</span> : null}
              <button
                type="button"
                aria-label={`Edit name and options of ${m.id}`}
                title="Edit name and options"
                disabled={disabled}
                onClick={() => setEditing((e) => (e === m.id ? null : m.id))}
                className="grid size-5 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-fg disabled:opacity-40"
              >
                <Pencil className="size-3" />
              </button>
              <Remove label={`Remove ${m.id}`} disabled={disabled} onClick={() => void onChange(models.filter((x) => x.id !== m.id))} />
            </li>
          ))}
        </ul>
      )}
      {edited && (
        <CustomModelEditor
          key={edited.id}
          provider={base.id}
          info={info}
          model={edited}
          onSave={(m) => onChange(models.map((x) => (x.id === m.id ? m : x)))}
          onCancel={() => setEditing(null)}
        />
      )}
      <form
        className="flex flex-wrap gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          void add();
        }}
      >
        <input
          aria-label={`${base.name} model slug`}
          value={slug}
          onChange={(e) => {
            setSlug(e.target.value);
            setProblem(null);
          }}
          placeholder="model-slug"
          disabled={disabled}
          spellCheck={false}
          className={cn(field, "min-w-32 flex-1 font-mono text-[12px]")}
        />
        <input aria-label={`${base.name} model name`} value={name} onChange={(e) => setName(e.target.value)} placeholder="Name (optional)" disabled={disabled} className={cn(field, "w-36")} />
        <button type="submit" disabled={disabled} className={smallBtn}>
          <Plus className="size-3.5" /> Add
        </button>
      </form>
      {problem && (
        <p role="alert" className="text-[12px] text-danger">
          {problem}
        </p>
      )}
    </div>
  );
}

// ── new threads, git ──

const INHERITS = { machine: "Machine's", owner: "Owner's", provider: "Provider's" } as const;

/**
 * What new threads start with, as rows: the model, permissions, plan and workspace. Each can be
 * left to the next layer (project, then machine, then owner, then the provider): `inherits` names
 * whose default that is, so the choice says so.
 */
export function DefaultsRows({
  defaults,
  providers,
  disabled,
  onChange,
  rows,
  inherits,
}: {
  defaults: ThreadDefaults;
  providers: readonly ProviderInfo[];
  disabled: boolean;
  onChange: (next: ThreadDefaults) => void;
  rows: { model: Setting; permissions: Setting; mode: Setting; workspace: Setting };
  inherits: "machine" | "owner" | "provider";
}) {
  const whose = INHERITS[inherits];
  const patch = (p: Partial<ThreadDefaults>) => {
    const next: Record<string, unknown> = { ...defaults, ...p };
    for (const k of Object.keys(next)) if (next[k] === undefined) delete next[k];
    onChange(next as ThreadDefaults);
  };
  const installed = providers.filter((p) => p.installed);
  return (
    <>
      <SettingRow
        s={rows.model}
        onReset={defaults.provider && !disabled ? () => patch({ provider: undefined, model: undefined, effort: undefined, options: undefined }) : null}
        control={
          installed.length > 0 ? (
            <span className="rounded-lg border border-line-2 bg-panel-2 px-1">
              <ModelPicker
                providers={installed}
                side="bottom"
                disabled={disabled}
                value={{ provider: defaults.provider ?? "", model: defaults.model ?? null, effort: defaults.effort ?? null, options: defaults.options ?? {} }}
                onChange={(c) => patch({ provider: c.provider, model: c.model ?? undefined, effort: c.effort ?? undefined, options: Object.keys(c.options).length ? c.options : undefined })}
              />
            </span>
          ) : (
            <span className="text-[12px] text-subtle">No providers yet</span>
          )
        }
      />
      <SettingRow
        s={rows.permissions}
        control={
          <Select
            label="Permissions for new threads"
            className="w-40"
            disabled={disabled}
            value={defaults.runtime_mode ?? "default"}
            onChange={(v) => patch({ runtime_mode: v === "default" ? undefined : (v as RuntimeMode) })}
            options={[{ value: "default", label: whose }, ...RUNTIME]}
          />
        }
      />
      <SettingRow
        s={rows.mode}
        control={
          <Segmented
            label="Interaction mode for new threads"
            value={defaults.interaction_mode ?? "default_mode"}
            onChange={(v) => patch({ interaction_mode: v === "plan" ? "plan" : v === "default" ? "default" : undefined })}
            options={[
              { value: "default_mode", label: whose },
              { value: "default", label: "Off" },
              { value: "plan", label: "Plan first" },
            ]}
          />
        }
      />
      <SettingRow
        s={rows.workspace}
        control={
          <Segmented
            label="Where new threads work"
            value={defaults.worktree === undefined ? "default" : defaults.worktree ? "worktree" : "checkout"}
            onChange={(v) => patch({ worktree: v === "default" ? undefined : v === "worktree" })}
            options={[
              // Past the owner it's a worktree when the project has a repo (new-thread.ts).
              { value: "default", label: inherits === "provider" ? "Auto" : whose },
              { value: "checkout", label: "Checkout" },
              { value: "worktree", label: "Worktree" },
            ]}
          />
        }
      />
    </>
  );
}

function NewThreads({ ctx }: { ctx: Ctx }) {
  const rows = { model: S.model, permissions: S.permissions, mode: S.mode, workspace: S.workspace };
  return (
    <SettingsSection title="New threads" ids={Object.values(rows).map((r) => r.id)}>
      <DefaultsRows
        inherits="owner"
        rows={rows}
        defaults={ctx.set.defaults ?? {}}
        providers={ctx.machine.info?.providers ?? []}
        disabled={!ctx.editable}
        onChange={(defaults) => void ctx.save(withFields(ctx.set, { defaults }))}
      />
    </SettingsSection>
  );
}

function GitIdentity({ ctx }: { ctx: Ctx }) {
  const { set, config, editable } = ctx;
  const [name, setName] = useState(set.git_name ?? "");
  const [email, setEmail] = useState(set.git_email ?? "");
  const dirty = name.trim() !== (set.git_name ?? "") || email.trim() !== (set.git_email ?? "");
  const both = !!name.trim() === !!email.trim();
  const base = [config?.base.git_name, config?.base.git_email].filter(Boolean).join(" · ");
  return (
    <SettingRow
      s={S.git}
      onReset={
        set.git_name !== undefined && editable
          ? async () => {
              if (!(await ctx.save(withFields(set, { git_name: undefined, git_email: undefined })))) return;
              setName("");
              setEmail("");
            }
          : null
      }
    >
      <form
        className="mt-2 flex flex-wrap gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          if (dirty && both) void ctx.save(withFields(set, { git_name: name.trim() || undefined, git_email: email.trim() || undefined }));
        }}
      >
        <input aria-label="Git name" value={name} onChange={(e) => setName(e.target.value)} placeholder={config?.base.git_name ?? "Name"} disabled={!editable} className={cn(field, "w-40")} />
        <input aria-label="Git email" type="email" value={email} onChange={(e) => setEmail(e.target.value)} placeholder={config?.base.git_email ?? "Email"} disabled={!editable} className={cn(field, "min-w-40 flex-1")} />
        <button type="submit" disabled={!editable || !dirty || !both} className={smallBtn}>
          Save
        </button>
      </form>
      {!both && <p className="mt-1.5 text-[12px] text-subtle">Both a name and an email, or neither.</p>}
      {base && <p className="mt-1.5 text-[11.5px] text-subtle">This machine's git config says {base}.</p>}
    </SettingRow>
  );
}
