"use client";

/**
 * Endpoints in the agents settings (docs/code/agents.md, "Endpoints"): a machine's endpoints and
 * each provider's "Endpoint" row (machine settings), the owner's profiles and applying one to
 * machines (owner settings), the editor both use, and applying a profile to a new sandbox once it
 * says hello. A key typed here is sealed in the browser to the one machine it's for (`keyChange`)
 * and sent only inside `endpoint.set`: it's never saved, shown, logged or kept in state once sent,
 * and a set key only ever shows as "Key set (never shown)".
 */

import { Check, KeyRound, Pencil, Plus, RefreshCw, Trash2, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { reachable, type EndpointCheck, type EndpointInfo, type EndpointKind, type EndpointProfile, type EndpointSpec, type KeyState, type Machine, type ProviderId, type Reply } from "@/lib/agents-api";
import type { AgentsClient } from "@/lib/agents-client";
import { checkEndpoint, endpointId, endpointOf, incompatible, KIND_LABEL, KIND_URL, keyChange, keyLabel } from "@/lib/agents-endpoints";
import { useEscapeLayer } from "./escape-layers";
import { GatewayEndpointItem } from "./gateway/machine";
import { MachineIcon } from "./identity";
import { field, Note, SettingRow, type Setting } from "./settings-ui";
import { ago } from "./shared";
import { cn, Segmented, Select, Switch } from "./ui";
import { Dialog, useKept } from "./ui/dialog";
import { useAgents, useAgentsSnapshot } from "./use-agents";

const KINDS: EndpointKind[] = ["cliproxy", "anthropic", "openai", "custom"];
/** The segmented control's words (the full ones are its tooltips). */
const KIND_SHORT: Record<EndpointKind, string> = { cliproxy: "CLIProxyAPI", anthropic: "Anthropic", openai: "OpenAI", custom: "Custom", gateway: "roll gateway" };

const TERMS = ["endpoint", "cliproxy", "cliproxyapi", "proxy", "gateway", "base url", "api key", "key", "token", "1password", "op://", "anthropic", "openai", "litellm", "openrouter"];

export const ENDPOINT_SETTINGS = {
  machine: {
    id: "endpoints",
    title: "Endpoints",
    section: "Endpoints",
    description: "Where its providers send requests instead of their own login. A key you type is sealed to this machine in your browser: roll only ever carries it encrypted, and never shows it again.",
    terms: TERMS,
  },
  owner: {
    id: "endpoints.profiles",
    title: "Endpoint profiles",
    section: "Endpoints",
    description: "Your endpoints without their keys, to set on any of your machines and on new sandboxes.",
    terms: [...TERMS, "profile", "sandbox", "apply", "default"],
  },
} satisfies Record<string, Setting>;

/** A provider card's "Endpoint" row, for the search. */
export const providerEndpointSetting = (p: { id: ProviderId; name: string }): Setting => ({
  id: `p.${p.id}.endpoint`,
  title: "Endpoint",
  section: p.name,
  description: "Its own login, or one of the machine's endpoints.",
  terms: ["endpoint", "proxy", "cliproxy", "gateway", "own login", "via", p.id],
});

// ── what the commands send ──

/** Every provider a machine has, on or off (what an endpoint may name). */
export const machineProviders = (m: Machine): { id: ProviderId; name: string }[] => m.info?.settings?.providers ?? m.info?.providers ?? [];

/** The spec part of an endpoint (no key state, models or check), its providers unset when none. */
export function specOf(e: EndpointSpec, providers: readonly ProviderId[] | undefined = e.providers): EndpointSpec {
  const out: EndpointSpec = { id: e.id, name: e.name, kind: e.kind, base_url: e.base_url };
  if (providers?.length) out.providers = [...providers];
  if (e.url_env) out.url_env = e.url_env;
  if (e.key_env) out.key_env = e.key_env;
  if (e.key_ref) out.key_ref = e.key_ref;
  if (e.fallback) out.fallback = true;
  return out;
}

/** `endpoint.set` to one machine, a typed key sealed to it (`undefined` keeps the key, `null` clears it). */
export async function setEndpointOn(client: AgentsClient, machine: Machine, spec: EndpointSpec, key?: string | null): Promise<void> {
  const change = await keyChange(machine.info?.seal_key, spec.id, key);
  await client.run(machine.id, { type: "endpoint.set", endpoint: spec, key: change }, "ok");
}

/**
 * A profile as one machine takes it: the same id, its providers cut to those the machine has and
 * can use, and that the gateway doesn't manage there (that's Gateway settings' to change).
 */
export function profileFor(profile: EndpointSpec, machine: Machine): EndpointSpec {
  const have = new Set(machineProviders(machine).map((p) => p.id));
  const gateway = (p: ProviderId) => profile.kind !== "gateway" && endpointOf(machine.info, p)?.kind === "gateway";
  return specOf(
    profile,
    (profile.providers ?? []).filter((p) => have.has(p) && !incompatible(p, profile.kind) && !gateway(p)),
  );
}

/** Why `machine`'s endpoints can't be changed from here, or `null`. An asleep sandbox wakes on a command. */
export function endpointsLocked(machine: Machine): string | null {
  if (!machine.info) return "Not connected yet: its endpoints can be set once it is.";
  if (!machine.info.seal_key || !machine.info.settings) return "Update roll on this machine to set endpoints from roll.";
  if (!reachable(machine)) return `Commands need ${machine.name} online: set its endpoints once it's back.`;
  return null;
}

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

export type PendingApply = { state: "waiting" } | { state: "applying" } | { state: "done"; machine: Machine } | { state: "failed"; error: string } | { state: "timeout" };

/** How long a new sandbox has to say hello before its profile is given up on. */
export const APPLY_WAIT_MS = 3 * 60_000;

/**
 * Apply a profile to a new sandbox once its daemon says hello with its seal key (at most
 * `timeout`), sealing `key` to it then. Returns `cancel`, which stops the wait and says whether it
 * did (`false` once `endpoint.set` is on its way).
 */
export function applyWhenReady(client: AgentsClient, machine: string, profile: EndpointSpec, key: string | undefined, onState: (p: PendingApply) => void, timeout = APPLY_WAIT_MS): () => boolean {
  let phase: "waiting" | "applying" | "over" = "waiting";
  let typed = key;
  let off = () => {};
  const stop = () => {
    off();
    clearTimeout(timer);
  };
  const timer = setTimeout(() => {
    if (phase !== "waiting") return;
    phase = "over";
    typed = undefined;
    stop();
    onState({ state: "timeout" });
  }, timeout);
  const check = () => {
    if (phase !== "waiting") return;
    const m = client.data.machines.get(machine)?.value;
    if (!m?.info?.seal_key || !m.info.settings) return;
    phase = "applying";
    stop();
    onState({ state: "applying" });
    const k = typed;
    typed = undefined;
    setEndpointOn(client, m, profileFor(profile, m), k).then(
      () => ((phase = "over"), onState({ state: "done", machine: m })),
      (e: unknown) => ((phase = "over"), onState({ state: "failed", error: message(e) })),
    );
  };
  off = client.subscribe(check);
  onState({ state: "waiting" });
  check();
  return () => {
    if (phase !== "waiting") return false;
    phase = "over";
    typed = undefined;
    stop();
    return true;
  };
}

// ── shared pieces ──

const iconBtn = "grid size-7 shrink-0 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg disabled:opacity-40 disabled:hover:bg-transparent";
const smallBtn = "flex h-7 shrink-0 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] shadow-card transition-colors hover:bg-fg/[0.06] disabled:opacity-50";
const textBtn = "rounded px-1 text-[12px] text-muted transition-colors hover:bg-fg/[0.06] hover:text-fg disabled:opacity-40";
const primaryBtn = "flex h-8 items-center gap-1.5 rounded-lg bg-fg px-3 text-[12.5px] font-medium text-bg shadow-card inset-shadow-highlight transition active:scale-[0.97] hover:opacity-90 disabled:opacity-50";

function Problem({ children }: { children: ReactNode }) {
  return (
    <p role="alert" className="text-[12px] text-danger">
      {children}
    </p>
  );
}

/** A key field: a password input that browsers don't fill or keep. */
function KeyInput({ label, value, onChange, placeholder, autoFocus, disabled }: { label: string; value: string; onChange: (v: string) => void; placeholder?: string; autoFocus?: boolean; disabled?: boolean }) {
  return (
    <input
      type="password"
      autoComplete="off"
      spellCheck={false}
      aria-label={label}
      value={value}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      autoFocus={autoFocus}
      disabled={disabled}
      data-1p-ignore
      data-lpignore="true"
      className={cn(field, "w-full font-mono text-[12px]")}
    />
  );
}

function Labeled({ label, children, hint }: { label: string; children: ReactNode; hint?: ReactNode }) {
  return (
    <div className="space-y-1.5">
      <p className="text-[11.5px] font-[510] text-muted">{label}</p>
      {children}
      {hint && <p className="text-[11.5px] leading-snug text-subtle">{hint}</p>}
    </div>
  );
}

// ── the editor ──

export interface EditorResult {
  spec: EndpointSpec;
  /** A typed key (sealed by the caller), `null` to clear it, `undefined` to keep it. */
  key: string | null | undefined;
}

/**
 * Adding or changing an endpoint (a machine's, with its key) or a profile (no key: it's typed when
 * the profile is applied). `onSave` answers an error to show (the daemon's or the relay's), or `null`.
 */
export function EndpointEditor({
  initial,
  keyState,
  taken,
  providers,
  uses,
  managed,
  withKey,
  replaceKey = false,
  pick,
  onSave,
  onCancel,
}: {
  initial?: EndpointSpec;
  /** Where its key comes from now (editing on a machine). */
  keyState?: KeyState;
  /** Ids already in use (a new one is made from the name). */
  taken: readonly string[];
  /** The providers that may use it. */
  providers: readonly { id: ProviderId; name: string }[];
  /** The endpoint a provider uses now, by name (choosing this one takes it from that one). */
  uses?: (p: ProviderId) => string | undefined;
  /** Providers the gateway manages on this machine: changed only in Gateway settings. */
  managed?: (p: ProviderId) => boolean;
  withKey: boolean;
  /** Open with the key field showing (Replace). */
  replaceKey?: boolean;
  /** A provider ticked from the start (New endpoint… from its card). */
  pick?: ProviderId;
  onSave: (r: EditorResult) => Promise<string | null>;
  onCancel: () => void;
}) {
  const [kind, setKind] = useState<EndpointKind>(initial?.kind ?? "cliproxy");
  const [name, setName] = useState(initial?.name ?? "");
  const [url, setUrl] = useState(initial?.base_url ?? "");
  const [picked, setPicked] = useState<ProviderId[]>(initial?.providers ?? (pick ? [pick] : []));
  const [urlEnv, setUrlEnv] = useState(initial?.url_env ?? "");
  const [keyEnv, setKeyEnv] = useState(initial?.key_env ?? "");
  const [fromOp, setFromOp] = useState(!!initial?.key_ref);
  const [ref, setRef] = useState(initial?.key_ref ?? "");
  const sealed = keyState === "sealed";
  // A new endpoint, or Replace: the field shows. Otherwise "Key set (never shown)" until Replace.
  const [typing, setTyping] = useState(!sealed || replaceKey);
  const [clearKey, setClearKey] = useState(false);
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const box = useRef<HTMLFormElement>(null);
  // Escape with something typed asks first (it threw the values away without a word); Cancel and
  // × are asked for, so they go at once. Escape again keeps editing.
  const dirty =
    kind !== (initial?.kind ?? "cliproxy") ||
    name !== (initial?.name ?? "") ||
    url !== (initial?.base_url ?? "") ||
    picked.join() !== (initial?.providers ?? (pick ? [pick] : [])).join() ||
    urlEnv !== (initial?.url_env ?? "") ||
    keyEnv !== (initial?.key_env ?? "") ||
    fromOp !== !!initial?.key_ref ||
    ref !== (initial?.key_ref ?? "") ||
    key !== "" ||
    clearKey;
  const [asking, setAsking] = useState(false);
  // Keep editing goes back to the field that was being typed in.
  const typedIn = useRef<HTMLElement | null>(null);
  const ask = () => {
    typedIn.current = document.activeElement instanceof HTMLElement && box.current?.contains(document.activeElement) ? document.activeElement : null;
    setAsking(true);
  };
  const keepEditing = () => {
    setAsking(false);
    setTimeout(() => typedIn.current?.focus({ preventScroll: true }), 0);
  };
  useEscapeLayer(true, () => (dirty ? ask() : onCancel()));
  useEscapeLayer(asking, keepEditing);
  const keepButton = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (asking) keepButton.current?.focus({ preventScroll: true });
  }, [asking]);
  // Braces: Chromium's scrollIntoView answers a promise, and an effect may only return its cleanup.
  useEffect(() => {
    box.current?.scrollIntoView?.({ block: "nearest", behavior: "smooth" });
  }, []);

  const chooseKind = (k: EndpointKind) => {
    setKind(k);
    setProblem(null);
    setPicked((ps) => ps.filter((p) => !incompatible(p, k)));
  };

  const save = async () => {
    // Left empty, it's named for its kind (the field's placeholder), not refused with the daemon's rule.
    const named = name.trim() || KIND_LABEL[kind];
    const spec: EndpointSpec = {
      id: initial?.id ?? endpointId(named, taken),
      name: named,
      kind,
      base_url: url,
      ...(picked.length ? { providers: picked } : {}),
      ...(kind === "custom" ? { url_env: urlEnv, key_env: keyEnv } : {}),
      ...(fromOp ? { key_ref: ref } : {}),
    };
    if (fromOp && !ref.trim()) return setProblem("Enter the op:// reference, or turn off From 1Password.");
    // A profile is for the providers it lists; one for none would never be used.
    if (!withKey && !picked.length && providers.length) return setProblem("Pick the providers this profile is for.");
    const checked = checkEndpoint(spec);
    if ("error" in checked) return setProblem(checked.error);
    let change: string | null | undefined;
    if (withKey) {
      if (fromOp) change = sealed ? null : undefined;
      else if (clearKey) change = null;
      else change = key.trim() || undefined;
    }
    setBusy(true);
    setProblem(null);
    const refused = await onSave({ spec: checked.spec, key: change });
    setBusy(false);
    if (refused) setProblem(refused);
    else setKey("");
  };

  const placeholder = KIND_URL[kind];
  return (
    <form
      ref={box}
      aria-label={initial ? `Edit ${initial.name}` : withKey ? "New endpoint" : "New profile"}
      className="space-y-3.5 bg-panel-2/60 px-3.5 py-3.5"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <div className="flex items-center gap-2">
        <h4 className="flex-1 text-[13px] font-[590] tracking-[-0.01em]">{initial ? `Edit ${initial.name}` : withKey ? "New endpoint" : "New profile"}</h4>
        <button type="button" onClick={onCancel} aria-label="Cancel" className={iconBtn}>
          <X className="size-3.5" />
        </button>
      </div>
      <Labeled label="Kind" hint={kindHint(kind)}>
        <Segmented label="Endpoint kind" value={kind} onChange={chooseKind} className="max-w-full flex-wrap" options={KINDS.map((k) => ({ value: k, label: KIND_SHORT[k], title: KIND_LABEL[k] }))} />
      </Labeled>
      <div className="grid gap-3 sm:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <Labeled label="Name">
          <input
            aria-label="Endpoint name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder={KIND_LABEL[kind]}
            spellCheck={false}
            // A new one starts at its name (New endpoint… from a provider's card too).
            autoFocus={!initial && !replaceKey}
            className={cn(field, "w-full")}
          />
        </Labeled>
        <Labeled label="Base URL">
          <input aria-label="Base URL" value={url} onChange={(e) => setUrl(e.target.value)} placeholder={placeholder} spellCheck={false} inputMode="url" className={cn(field, "w-full font-mono text-[12px]")} />
        </Labeled>
      </div>
      {kind === "custom" && (
        <div className="grid gap-3 sm:grid-cols-2">
          <Labeled label="Variable for the URL">
            <input aria-label="URL variable" value={urlEnv} onChange={(e) => setUrlEnv(e.target.value)} placeholder="GOOGLE_GEMINI_BASE_URL" spellCheck={false} className={cn(field, "w-full font-mono text-[12px]")} />
          </Labeled>
          <Labeled label="Variable for the key">
            <input aria-label="Key variable" value={keyEnv} onChange={(e) => setKeyEnv(e.target.value)} placeholder="GEMINI_API_KEY" spellCheck={false} className={cn(field, "w-full font-mono text-[12px]")} />
          </Labeled>
        </div>
      )}
      <Labeled label="Used by">
        {providers.length ? (
          <ul className="space-y-0.5" aria-label="Providers that use it">
            {providers.map((p) => {
              const why = incompatible(p.id, kind) ?? (kind !== "gateway" && managed?.(p.id) ? "Managed in Gateway settings" : null);
              const other = uses?.(p.id);
              const now = other && other !== initial?.name ? other : undefined;
              return (
                <li key={p.id}>
                  <label className={cn("flex min-w-0 items-center gap-2 rounded-md px-1 py-1 text-[12.5px]", why ? "cursor-not-allowed text-subtle" : "cursor-pointer hover:bg-fg/[0.04]")}>
                    <input
                      type="checkbox"
                      checked={!why && picked.includes(p.id)}
                      disabled={!!why}
                      onChange={(e) => setPicked((ps) => (e.target.checked ? [...ps, p.id] : ps.filter((x) => x !== p.id)))}
                      className="accent-[var(--accent)]"
                    />
                    <span className="shrink-0">{p.name}</span>
                    {why ? <span className="min-w-0 truncate text-[11.5px] text-subtle">{why}</span> : now && <span className="min-w-0 truncate text-[11.5px] text-subtle">uses {now} now</span>}
                  </label>
                </li>
              );
            })}
          </ul>
        ) : (
          <p className="text-[12px] text-subtle">No providers reported yet.</p>
        )}
      </Labeled>
      <Labeled
        label={withKey ? "Key" : "Key from 1Password"}
        hint={
          fromOp
            ? "Read on the machine with the op CLI when it's needed. A reference isn't a secret."
            : withKey
              ? typing && !clearKey
                ? "Sealed to this machine in your browser; roll only carries it encrypted and never shows it again."
                : null
              : "A profile never holds a key: you type it when you apply the profile, sealed to each machine separately."
        }
      >
        <div className="flex flex-wrap items-center gap-2">
          {fromOp ? (
            <input aria-label="1Password reference" value={ref} onChange={(e) => setRef(e.target.value)} placeholder="op://vault/item/field" spellCheck={false} className={cn(field, "min-w-48 flex-1 font-mono text-[12px]")} />
          ) : withKey ? (
            typing && !clearKey ? (
              <div className="min-w-48 flex-1">
                <KeyInput label="Key" value={key} onChange={setKey} placeholder={sealed ? "New key" : "Key (optional)"} autoFocus={replaceKey} />
              </div>
            ) : (
              <span className="flex min-w-0 flex-1 items-center gap-2 text-[12.5px]">
                <KeyRound className="size-3.5 shrink-0 text-subtle" aria-hidden />
                <span className={cn(clearKey && "text-subtle line-through")}>Key set (never shown)</span>
                {clearKey ? (
                  <button type="button" className={textBtn} onClick={() => setClearKey(false)}>
                    Keep it
                  </button>
                ) : (
                  <>
                    <button type="button" className={textBtn} onClick={() => setTyping(true)}>
                      Replace
                    </button>
                    <button type="button" className={textBtn} onClick={() => setClearKey(true)}>
                      Remove
                    </button>
                  </>
                )}
              </span>
            )
          ) : null}
          <label className="ml-auto flex shrink-0 items-center gap-2 text-[12px] text-muted">
            From 1Password
            <Switch size="sm" checked={fromOp} onCheckedChange={setFromOp} aria-label="Key from 1Password" />
          </label>
        </div>
        {clearKey && <p className="text-[11.5px] text-subtle">The key is removed when you save.</p>}
      </Labeled>
      {problem && <Problem>{problem}</Problem>}
      {asking ? (
        <div role="alertdialog" aria-label="Discard your changes?" className="flex flex-wrap items-center gap-2 rounded-lg border border-line-2 bg-panel-2 px-2.5 py-2 text-[12px]">
          <span className="min-w-0 flex-1 text-muted">Discard what you typed?</span>
          <button type="button" ref={keepButton} onClick={keepEditing} className="h-7 rounded-md px-2.5 text-[12px] text-muted hover:text-fg">
            Keep editing
          </button>
          <button type="button" onClick={onCancel} className="h-7 rounded-md bg-danger/10 px-2.5 text-[12px] font-medium text-danger hover:bg-danger/20">
            Discard
          </button>
        </div>
      ) : (
      <div className="flex items-center justify-end gap-2">
        <button type="button" onClick={onCancel} className="h-8 rounded-lg px-3 text-[12.5px] text-muted hover:text-fg">
          Cancel
        </button>
        <button type="submit" disabled={busy} className={primaryBtn}>
          {busy ? <span className="shimmer">Saving…</span> : initial ? "Save" : withKey ? "Add endpoint" : "Add profile"}
        </button>
      </div>
      )}
    </form>
  );
}

function kindHint(kind: EndpointKind): string {
  switch (kind) {
    case "cliproxy":
      return "One server for Claude Code, Codex and Gemini CLI: each gets the routes it speaks.";
    case "anthropic":
      return "ANTHROPIC_BASE_URL, with the key as a bearer token. Not for Codex.";
    case "openai":
      return "OPENAI_BASE_URL (include /v1) and OPENAI_API_KEY. Not for Claude Code.";
    case "custom":
      return "The URL and the key go in the variables you name.";
    case "gateway":
      return "roll's model gateway on another of your computers: set from Gateway settings.";
  }
}

// ── a machine's endpoints ──

/** What the machine settings ask the Endpoints section to open. */
export interface EndpointEdit {
  /** An endpoint's id, or none for a new one. */
  id?: string;
  /** Tick this provider in a new one. */
  provider?: ProviderId;
  /** Open on the key field (Replace). */
  key?: boolean;
}

export function MachineEndpoints({ machine, editing, onEdit }: { machine: Machine; editing: EndpointEdit | null; onEdit: (e: EndpointEdit | null) => void }) {
  const { client, services, owner } = useAgents();
  const list = machine.info?.settings?.endpoints ?? [];
  const locked = endpointsLocked(machine);
  const providers = machineProviders(machine);
  const [profiles, setProfiles] = useState<Set<string>>(new Set());
  useEffect(() => {
    let live = true;
    services?.endpoints
      .list(owner)
      .then((ps) => live && setProfiles(new Set(ps.map((p) => p.id))))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [services, owner]);
  const name = (p: ProviderId) => providers.find((x) => x.id === p)?.name ?? p;
  const save = async (r: EditorResult) => {
    try {
      await setEndpointOn(client, machine, r.spec, r.key);
      onEdit(null);
      return null;
    } catch (e) {
      return message(e);
    }
  };
  const editor = (initial?: EndpointInfo) => (
    <EndpointEditor
      key={initial?.id ?? "new"}
      initial={initial}
      keyState={initial?.key}
      replaceKey={editing?.key}
      pick={editing?.provider}
      taken={list.map((e) => e.id)}
      providers={providers}
      uses={(p) => endpointOf(machine.info, p)?.name}
      managed={(p) => endpointOf(machine.info, p)?.kind === "gateway"}
      withKey
      onSave={save}
      onCancel={() => onEdit(null)}
    />
  );
  return (
    <>
      <SettingRow
        s={ENDPOINT_SETTINGS.machine}
        control={
          <button type="button" disabled={!!locked || (editing !== null && !editing.id)} onClick={() => onEdit({})} className={smallBtn}>
            <Plus className="size-3.5" /> Add endpoint
          </button>
        }
      >
        {locked && <p className="mt-2 rounded-lg border border-line bg-panel-2 px-2.5 py-1.5 text-[12px] text-subtle">{locked}</p>}
      </SettingRow>
      {editing && !editing.id && !locked && editor()}
      {list.length === 0 && !(editing && !editing.id) && <p className="px-3.5 py-3 text-[12.5px] text-subtle">No endpoints: every provider uses its own login here.</p>}
      {list.map((e) =>
        // roll's gateway sets this one when the machine joins: read-only here, managed in Gateway settings.
        e.kind === "gateway" ? (
          <GatewayEndpointItem key={e.id} machine={machine} endpoint={e} providerName={name} />
        ) : editing?.id === e.id && !locked ? (
          editor(e)
        ) : (
          <EndpointItem key={e.id} machine={machine} endpoint={e} profile={profiles.has(e.id)} locked={!!locked} providerName={name} onEdit={(key) => onEdit({ id: e.id, key })} />
        ),
      )}
    </>
  );
}

function EndpointItem({
  machine,
  endpoint: e,
  profile,
  locked,
  providerName,
  onEdit,
}: {
  machine: Machine;
  endpoint: EndpointInfo;
  profile: boolean;
  locked: boolean;
  providerName: (p: ProviderId) => string;
  onEdit: (key: boolean) => void;
}) {
  const { client } = useAgents();
  const [check, setCheck] = useState<EndpointCheck | "busy" | null>(null);
  const [asking, setAsking] = useState(false);
  useEscapeLayer(asking, () => setAsking(false));
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const act = async (what: string, fn: () => Promise<unknown>) => {
    setBusy(what);
    setProblem(null);
    try {
      await fn();
    } catch (err) {
      setProblem(message(err));
    } finally {
      setBusy(null);
    }
  };
  const runCheck = async () => {
    setCheck("busy");
    setProblem(null);
    try {
      setCheck(await client.run<Extract<Reply, { type: "endpoint_checked" }>>(machine.id, { type: "endpoint.check", endpoint: e.id }, "endpoint_checked"));
    } catch (err) {
      setCheck({ ok: false, models: 0, ms: 0, error: message(err) });
    }
  };
  const users = (e.providers ?? []).map(providerName);
  const meta = [users.length ? users.join(", ") : "No provider uses it", e.kind !== "custom" && `${(e.models ?? []).length} models`, e.checked_at && `checked ${ago(e.checked_at)}`].filter(Boolean).join(" · ");
  return (
    <div className="px-3.5 py-3 text-[13px]" aria-label={`Endpoint ${e.name}`} role="group">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 flex-wrap items-center gap-1.5">
            <h4 className="truncate font-medium tracking-[-0.01em]">{e.name}</h4>
            <Note>{KIND_LABEL[e.kind]}</Note>
            {profile && <Note title="Set from your endpoint profile with this id">profile</Note>}
          </div>
          <p className="truncate font-mono text-[11.5px] text-muted" title={e.base_url}>
            {e.base_url}
          </p>
          <p className="mt-0.5 text-[12px] text-subtle">{meta}</p>
        </div>
        {!asking && (
          <div className="-mr-1 flex shrink-0 items-center">
            <button type="button" disabled={locked || check === "busy"} onClick={runCheck} className={iconBtn} aria-label={`Check ${e.name}`} title="Check: ask it for its models now">
              <RefreshCw className={cn("size-3.5", check === "busy" && "animate-spin motion-reduce:animate-none")} />
            </button>
            <button type="button" disabled={locked} onClick={() => onEdit(false)} className={iconBtn} aria-label={`Edit ${e.name}`} title="Edit">
              <Pencil className="size-3.5" />
            </button>
            <button type="button" disabled={locked} onClick={() => setAsking(true)} className={cn(iconBtn, "hover:text-danger")} aria-label={`Remove ${e.name}`} title="Remove">
              <Trash2 className="size-3.5" />
            </button>
          </div>
        )}
      </div>
      <div className="mt-2 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-[12px]">
        <KeyRound className="size-3.5 shrink-0 text-subtle" aria-hidden />
        <span className="min-w-0 truncate text-muted" title={e.key === "reference" ? e.key_ref : undefined}>
          {keyLabel(e)}
        </span>
        {e.key === "sealed" && (
          <>
            <button type="button" disabled={locked} className={textBtn} onClick={() => onEdit(true)} aria-label={`Replace ${e.name}'s key`}>
              Replace
            </button>
            <button type="button" disabled={locked || busy !== null} className={textBtn} onClick={() => act("key", () => setEndpointOn(client, machine, specOf(e), null))} aria-label={`Remove ${e.name}'s key`}>
              {busy === "key" ? <span className="shimmer">Removing…</span> : "Remove"}
            </button>
          </>
        )}
        {e.key === "none" && (
          <button type="button" disabled={locked} className={textBtn} onClick={() => onEdit(true)}>
            Add a key
          </button>
        )}
      </div>
      {check === "busy" ? (
        <p className="mt-1.5 text-[12px] text-subtle" role="status">
          <span className="shimmer">Checking…</span>
        </p>
      ) : check ? (
        <p className={cn("mt-1.5 flex items-center gap-1.5 text-[12px]", check.ok ? "text-muted" : "text-danger")} role="status">
          {check.ok && <Check className="size-3.5 text-ok" aria-hidden />}
          {check.ok ? `${check.models} models · ${check.ms} ms` : (check.error ?? "The check failed")}
        </p>
      ) : (
        e.error && <p className="mt-1.5 text-[12px] text-danger">{e.error}</p>
      )}
      {asking && (
        <div className="mt-2 flex flex-wrap items-center gap-2 rounded-lg border border-line-2 bg-panel-2 px-2.5 py-2 text-[12px]">
          <span className="min-w-0 flex-1 text-muted">Remove {e.name} from {machine.name}? Its providers go back to their own login.</span>
          <button
            type="button"
            disabled={busy !== null}
            onClick={() => act("remove", () => client.run(machine.id, { type: "endpoint.remove", endpoint: e.id }, "ok"))}
            className="h-7 rounded-md bg-danger/10 px-2.5 text-[12px] font-medium text-danger hover:bg-danger/20"
          >
            {busy === "remove" ? <span className="shimmer">Removing…</span> : `Remove ${e.name}`}
          </button>
          <button type="button" onClick={() => setAsking(false)} className={iconBtn} aria-label="Keep it">
            <X className="size-3.5" />
          </button>
        </div>
      )}
      {problem && (
        <div className="mt-1.5">
          <Problem>{problem}</Problem>
        </div>
      )}
    </div>
  );
}

/** A provider card's "Endpoint" row: its own login, one of the machine's endpoints, or a new one. */
export function ProviderEndpointRow({ machine, provider, onNew }: { machine: Machine; provider: { id: ProviderId; name: string }; onNew: () => void }) {
  const { client } = useAgents();
  const list = machine.info?.settings?.endpoints ?? [];
  const current = endpointOf(machine.info, provider.id);
  const locked = endpointsLocked(machine);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  // The gateway's endpoint is the relay's: a provider is moved onto or off it in Gateway settings.
  const managed = current?.kind === "gateway";
  const choose = async (v: string) => {
    if (v === "new") return onNew();
    const next = v === "own" ? null : list.find((e) => `e:${e.id}` === v);
    if (next === current) return;
    setBusy(true);
    setProblem(null);
    try {
      // Setting the one it moves to takes it from the other (the daemon's rule); to its own login,
      // the one it used drops it.
      if (next) await setEndpointOn(client, machine, specOf(next, [...(next.providers ?? []), provider.id]));
      else if (current) await setEndpointOn(client, machine, specOf(current, (current.providers ?? []).filter((p) => p !== provider.id)));
    } catch (e) {
      setProblem(message(e));
    } finally {
      setBusy(false);
    }
  };
  const s = providerEndpointSetting(provider);
  return (
    <SettingRow
      s={{ ...s, description: current ? `${KIND_LABEL[current.kind]} · ${current.base_url}` : "Its own login on this machine." }}
      note={busy ? <span className="shimmer text-[11px] text-subtle">Saving…</span> : undefined}
      control={
        <Select
          label={`${provider.name} endpoint`}
          className="w-44"
          disabled={!!locked || busy || managed}
          title={locked ?? (managed ? "Managed in Gateway settings" : `${provider.name} endpoint`)}
          value={current ? `e:${current.id}` : "own"}
          onChange={(v) => void choose(v)}
          options={[
            { value: "own", label: "Own login" },
            ...list
              .filter((e) => !incompatible(provider.id, e.kind) && (e.kind !== "gateway" || e === current))
              .map((e) => ({ value: `e:${e.id}`, label: e.name, hint: e.name === KIND_SHORT[e.kind] ? undefined : KIND_SHORT[e.kind] })),
            { value: "new", label: "New endpoint…", icon: <Plus className="size-3.5 text-subtle" /> },
          ]}
        />
      }
    >
      {problem && (
        <div className="mt-1.5">
          <Problem>{problem}</Problem>
        </div>
      )}
    </SettingRow>
  );
}

// ── the owner's profiles ──

export function EndpointProfilesSection() {
  const { client, services, owner } = useAgents();
  const data = useAgentsSnapshot(client)?.data ?? client.data;
  const api = services?.endpoints;
  const machines = useMemo(() => [...data.machines.values()].map((r) => r.value).sort((a, b) => a.name.localeCompare(b.name)), [data.machines]);
  const [profiles, setProfiles] = useState<EndpointProfile[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [asking, setAsking] = useState<string | null>(null);
  useEscapeLayer(asking !== null, () => setAsking(null));
  const [applying, setApplying] = useState<EndpointProfile | null>(null);
  const apply = useKept(applying);
  const load = async () => {
    if (!api) return;
    try {
      setProfiles(await api.list(owner));
    } catch (e) {
      setProblem(message(e));
    }
  };
  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [api, owner]);
  // Every provider on the owner's machines, once.
  const providers = useMemo(() => {
    const byId = new Map<ProviderId, { id: ProviderId; name: string }>();
    for (const m of machines) for (const p of machineProviders(m)) if (!byId.has(p.id)) byId.set(p.id, { id: p.id, name: p.name });
    for (const p of profiles ?? []) for (const id of p.providers ?? []) if (!byId.has(id)) byId.set(id, { id, name: id });
    return [...byId.values()];
  }, [machines, profiles]);
  const name = (id: ProviderId) => providers.find((p) => p.id === id)?.name ?? id;

  const save = async (profile: Omit<EndpointProfile, "updated_at">) => {
    if (!api) return "Sign in to keep endpoint profiles.";
    try {
      await api.save(owner, profile);
      await load();
      return null;
    } catch (e) {
      return message(e);
    }
  };
  const remove = async (id: string) => {
    if (!api) return;
    setProblem(null);
    try {
      await api.remove(owner, id);
      setAsking(null);
      await load();
    } catch (e) {
      setProblem(message(e));
    }
  };
  const editor = (initial?: EndpointProfile) => (
    <EndpointEditor
      key={initial?.id ?? "new"}
      initial={initial}
      taken={(profiles ?? []).map((p) => p.id)}
      providers={providers}
      withKey={false}
      onSave={async (r) => {
        const refused = await save({ ...r.spec, ...(initial?.sandbox_default ? { sandbox_default: true } : {}) });
        if (!refused) setEditing(null);
        return refused;
      }}
      onCancel={() => setEditing(null)}
    />
  );
  return (
    <>
      <SettingRow
        s={ENDPOINT_SETTINGS.owner}
        control={
          <button type="button" disabled={!api || editing === "new"} onClick={() => setEditing("new")} className={smallBtn}>
            <Plus className="size-3.5" /> Add profile
          </button>
        }
      >
        {problem && (
          <div className="mt-2">
            <Problem>{problem}</Problem>
          </div>
        )}
      </SettingRow>
      {editing === "new" && editor()}
      {profiles === null && api && (
        <p className="px-3.5 py-3 text-[12.5px] text-subtle">
          <span className="shimmer">Loading…</span>
        </p>
      )}
      {profiles?.length === 0 && editing !== "new" && <p className="px-3.5 py-3 text-[12.5px] text-subtle">No profiles yet. Add one to set the same endpoint on several machines.</p>}
      {(profiles ?? []).map((p) =>
        editing === p.id ? (
          editor(p)
        ) : (
          <div key={p.id} role="group" aria-label={`Profile ${p.name}`} className="px-3.5 py-3 text-[13px]">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                  <h4 className="truncate font-medium tracking-[-0.01em]">{p.name}</h4>
                  <Note>{KIND_LABEL[p.kind]}</Note>
                </div>
                <p className="truncate font-mono text-[11.5px] text-muted" title={p.base_url}>
                  {p.base_url}
                </p>
                <p className="mt-0.5 text-[12px] text-subtle">
                  {[(p.providers ?? []).map(name).join(", ") || "No providers", p.key_ref ? `1Password: ${p.key_ref}` : "Key typed when applied"].join(" · ")}
                </p>
              </div>
              {asking !== p.id && (
                <div className="-mr-1 flex shrink-0 items-center">
                  <button type="button" onClick={() => setEditing(p.id)} className={iconBtn} aria-label={`Edit ${p.name}`} title="Edit">
                    <Pencil className="size-3.5" />
                  </button>
                  <button type="button" onClick={() => setAsking(p.id)} className={cn(iconBtn, "hover:text-danger")} aria-label={`Remove ${p.name}`} title="Remove">
                    <Trash2 className="size-3.5" />
                  </button>
                </div>
              )}
            </div>
            <div className="mt-2.5 flex flex-wrap items-center gap-x-4 gap-y-2">
              <button type="button" onClick={() => setApplying(p)} className={smallBtn}>
                Apply to machines…
              </button>
              <label className="flex items-center gap-2 text-[12px] text-muted">
                <Switch
                  size="sm"
                  checked={!!p.sandbox_default}
                  onCheckedChange={(on) => void save({ ...specOf(p), ...(on ? { sandbox_default: true } : {}) }).then((refused) => refused && setProblem(refused))}
                  aria-label={`${p.name}: default for new sandboxes`}
                />
                Default for new sandboxes
              </label>
            </div>
            {asking === p.id && (
              <div className="mt-2 flex flex-wrap items-center gap-2 rounded-lg border border-line-2 bg-panel-2 px-2.5 py-2 text-[12px]">
                <span className="min-w-0 flex-1 text-muted">Remove the profile? Machines it was applied to keep their endpoint.</span>
                <button type="button" onClick={() => void remove(p.id)} className="h-7 rounded-md bg-danger/10 px-2.5 text-[12px] font-medium text-danger hover:bg-danger/20">
                  Remove {p.name}
                </button>
                <button type="button" onClick={() => setAsking(null)} className={iconBtn} aria-label="Keep it">
                  <X className="size-3.5" />
                </button>
              </div>
            )}
          </div>
        ),
      )}
      {apply.value && <ApplyDialog key={apply.key} open={apply.open} onExited={apply.onExited} profile={apply.value} machines={machines} onClose={() => setApplying(null)} />}
    </>
  );
}

type Outcome = "busy" | { ok: true } | { error: string };

/**
 * "Apply to machines…": the machines that can take it (a seal key, and online or a sandbox), the
 * key typed once and sealed to each machine separately, and how each one went.
 */
function ApplyDialog({ profile, machines, onClose, open = true, onExited }: { profile: EndpointProfile; machines: Machine[]; onClose: () => void; open?: boolean; onExited?: () => void }) {
  const { client } = useAgents();
  const [picked, setPicked] = useState<Set<string>>(() => new Set(machines.filter((m) => !endpointsLocked(m)).map((m) => m.id)));
  const [key, setKey] = useState("");
  const [outcomes, setOutcomes] = useState<Map<string, Outcome>>(new Map());
  const [sent, setSent] = useState(false);
  const busy = [...outcomes.values()].some((o) => o === "busy");
  const chosen = machines.filter((m) => picked.has(m.id) && !endpointsLocked(m));

  const apply = async () => {
    const typed = profile.key_ref ? undefined : key.trim() || undefined;
    setKey("");
    setSent(true);
    setOutcomes(new Map(chosen.map((m) => [m.id, "busy"])));
    const done = (id: string, o: Outcome) => setOutcomes((all) => new Map(all).set(id, o));
    await Promise.all(
      chosen.map(async (m) => {
        try {
          // Sealed to each machine on its own: one box opens on one daemon only.
          await setEndpointOn(client, m, profileFor(profile, m), typed);
          done(m.id, { ok: true });
        } catch (e) {
          done(m.id, { error: message(e) });
        }
      }),
    );
  };

  // The shared dialog: focus kept inside and given back; Escape, a press outside and × wait while
  // it applies; it shrinks back out before it unmounts.
  return (
    <Dialog
      open={open}
      onClose={onClose}
      onExited={onExited}
      busy={busy}
      className="sm:w-[min(460px,100%)]"
      title={`Apply ${profile.name}`}
      lead={
        <>
          {KIND_LABEL[profile.kind]} at <span className="font-mono text-[12px]">{profile.base_url}</span>, set on each machine you pick with the same id.
        </>
      }
      footer={
        sent ? (
          <button type="button" disabled={busy} onClick={onClose} className={primaryBtn}>
            Done
          </button>
        ) : (
          <>
            <button type="button" onClick={onClose} className="h-8 rounded-lg px-3 text-[12.5px] text-muted hover:text-fg">
              Cancel
            </button>
            <button type="button" disabled={chosen.length === 0} onClick={() => void apply()} className={primaryBtn}>
              {chosen.length === 1 ? "Apply to 1 machine" : `Apply to ${chosen.length} machines`}
            </button>
          </>
        )
      }
    >
      <div className="space-y-4">
        <ul className="divide-y divide-line overflow-hidden rounded-xl border border-line-2 bg-panel inset-shadow-highlight" aria-label="Machines">
          {machines.map((m) => {
            const why = endpointsLocked(m);
            const o = outcomes.get(m.id);
            const has = m.info?.settings?.endpoints?.some((e) => e.id === profile.id);
            return (
              <li key={m.id}>
                <label className={cn("flex min-w-0 items-center gap-2.5 px-3 py-2 text-[13px]", why ? "cursor-not-allowed" : "cursor-pointer hover:bg-fg/[0.03]")}>
                  <input
                    type="checkbox"
                    disabled={!!why || sent}
                    checked={!why && picked.has(m.id)}
                    onChange={(e) =>
                      setPicked((s) => {
                        const n = new Set(s);
                        if (e.target.checked) n.add(m.id);
                        else n.delete(m.id);
                        return n;
                      })
                    }
                    className="accent-[var(--accent)]"
                    aria-label={m.name}
                  />
                  <MachineIcon machine={m} tile className="size-5" />
                  <span className={cn("min-w-0 truncate", why && "text-subtle")}>{m.name}</span>
                  <span className="ml-auto min-w-0 shrink truncate text-right text-[11.5px]" role={o && o !== "busy" ? "status" : undefined}>
                    {o === "busy" ? (
                      <span className="shimmer text-subtle">Applying…</span>
                    ) : o && "ok" in o ? (
                      <span className="inline-flex items-center gap-1 text-ok">
                        <Check className="size-3.5" /> Applied
                      </span>
                    ) : o ? (
                      <span className="text-danger" title={o.error}>
                        {o.error}
                      </span>
                    ) : why ? (
                      <span className="text-subtle" title={why}>
                        {!m.info?.seal_key ? "Needs a newer roll" : "Offline"}
                      </span>
                    ) : (
                      has && <span className="text-subtle">Has it: replaced</span>
                    )}
                  </span>
                </label>
              </li>
            );
          })}
        </ul>
        {!profile.key_ref && !sent && (
          <Labeled label="Key (optional)" hint="Typed once, sealed in your browser to each machine separately. Empty: each machine keeps the key it has.">
            <KeyInput label="Key" value={key} onChange={setKey} placeholder="Key" />
          </Labeled>
        )}
      </div>
    </Dialog>
  );
}
