/**
 * The owner's agents settings: what follows the user to every device (docs/code/agents.md,
 * "Owner settings"), T3 Code's client settings that roll has a use for (`ClientSettingsSchema` in
 * `packages/contracts/src/settings.ts` there): notifications, the send key, what a message to a
 * running thread does, confirming deletes, and what new threads start with. roll keeps them as an
 * opaque document per owner (`GET`/`PUT …/settings`, `AgentsServices.settings`); this module is
 * their schema. Keys it doesn't know (a newer dashboard's) are kept as they are.
 */

import { useCallback, useEffect, useSyncExternalStore } from "react";
import { SettingsConflict, type ProviderId, type SendMode, type ThreadDefaults } from "./agents-api";
import type { AgentsServices } from "./agents-services";

/** T3's `notificationMode`: when an agent needs you or finishes while the page isn't in front. */
export type NotifyMode = "off" | "notify" | "sound" | "both";
/** T3's `sendShortcut`: Enter sends (Shift+Enter breaks the line), or ⌘/Ctrl+Enter sends. */
export type SendKey = "enter" | "mod_enter";

export interface OwnerSettings {
  notifications: NotifyMode;
  send: SendKey;
  /** What a message to a running thread does unless flipped (T3's `followUpBehavior`). */
  follow_up: SendMode;
  /** Ask before deleting a thread (T3's `confirmThreadDelete`). */
  confirm_delete: boolean;
  /** What new threads start with, after their project's and machine's defaults. */
  defaults: ThreadDefaults;
}

export const DEFAULT_OWNER_SETTINGS: OwnerSettings = { notifications: "off", send: "enter", follow_up: "queue", confirm_delete: true, defaults: {} };

const one = <T extends string>(v: unknown, of: readonly T[], def: T): T => (of.includes(v as T) ? (v as T) : def);
const RUNTIME = ["full_access", "auto_edit", "ask"] as const;

function parseDefaults(v: unknown): ThreadDefaults {
  if (!v || typeof v !== "object" || Array.isArray(v)) return {};
  const d = v as Record<string, unknown>;
  const s = (x: unknown) => (typeof x === "string" && x.trim() ? x : undefined);
  const out: ThreadDefaults = {
    provider: s(d.provider),
    model: s(d.model),
    effort: s(d.effort),
    runtime_mode: RUNTIME.includes(d.runtime_mode as never) ? (d.runtime_mode as ThreadDefaults["runtime_mode"]) : undefined,
    interaction_mode: d.interaction_mode === "plan" || d.interaction_mode === "default" ? d.interaction_mode : undefined,
    worktree: typeof d.worktree === "boolean" ? d.worktree : undefined,
  };
  // A model belongs to its provider.
  if (!out.provider) {
    delete out.model;
    delete out.effort;
  }
  return Object.fromEntries(Object.entries(out).filter(([, x]) => x !== undefined)) as ThreadDefaults;
}

/** The settings in a saved document, every bad or missing value its default. */
export function parseOwnerSettings(doc: unknown): OwnerSettings {
  const d = doc && typeof doc === "object" && !Array.isArray(doc) ? (doc as Record<string, unknown>) : {};
  return {
    notifications: one(d.notifications, ["off", "notify", "sound", "both"], DEFAULT_OWNER_SETTINGS.notifications),
    send: one(d.send, ["enter", "mod_enter"], DEFAULT_OWNER_SETTINGS.send),
    follow_up: one(d.follow_up, ["queue", "steer"], DEFAULT_OWNER_SETTINGS.follow_up),
    confirm_delete: typeof d.confirm_delete === "boolean" ? d.confirm_delete : DEFAULT_OWNER_SETTINGS.confirm_delete,
    defaults: parseDefaults(d.defaults),
  };
}

/**
 * What new threads start with: each field from the first layer that sets it (the project's
 * defaults, then the machine's, then the owner's). The provider, model and effort go together,
 * from the first layer naming a provider in `available` (when given).
 */
export function newThreadDefaults(layers: readonly (ThreadDefaults | undefined)[], available?: readonly ProviderId[]): ThreadDefaults {
  const out: ThreadDefaults = {};
  const pick = layers.find((l) => l?.provider && (!available || available.includes(l.provider)));
  if (pick?.provider) {
    out.provider = pick.provider;
    if (pick.model) out.model = pick.model;
    if (pick.effort) out.effort = pick.effort;
  }
  for (const k of ["runtime_mode", "interaction_mode", "worktree"] as const) {
    const l = layers.find((x) => x?.[k] !== undefined);
    if (l) (out as Record<string, unknown>)[k] = l[k];
  }
  return out;
}

// ── one store per owner, shared by the header (notifications) and the surface ──

export interface OwnerSettingsState {
  settings: OwnerSettings;
  /** The saved document's revision (0: never saved). */
  revision: number;
  loaded: boolean;
  error: string | null;
}

type Docs = AgentsServices["settings"];

class Store {
  state: OwnerSettingsState = { settings: DEFAULT_OWNER_SETTINGS, revision: 0, loaded: false, error: null };
  /** The whole document as saved, so keys this dashboard doesn't know go back as they came. */
  private doc: Record<string, unknown> = {};
  private listeners = new Set<() => void>();
  private loading: Promise<void> | null = null;

  constructor(
    private docs: Docs,
    private owner: string,
  ) {}

  subscribe = (f: () => void) => {
    this.listeners.add(f);
    return () => this.listeners.delete(f);
  };
  get = () => this.state;

  private set(patch: Partial<OwnerSettingsState>) {
    this.state = { ...this.state, ...patch };
    for (const f of this.listeners) f();
  }

  private take(saved: { doc: unknown; revision: number } | null) {
    this.doc = saved && typeof saved.doc === "object" && saved.doc && !Array.isArray(saved.doc) ? { ...(saved.doc as Record<string, unknown>) } : {};
    this.set({ settings: parseOwnerSettings(this.doc), revision: saved?.revision ?? 0, loaded: true, error: null });
  }

  load(): Promise<void> {
    this.loading ??= this.docs
      .load(this.owner)
      .then((s) => this.take(s))
      .catch((e: unknown) => this.set({ loaded: true, error: e instanceof Error ? e.message : String(e) }))
      .finally(() => {
        this.loading = null;
      });
    return this.loading;
  }

  /** Save a change over the revision this device has; over a newer one, load it and apply the change again (once). */
  async save(patch: Partial<OwnerSettings>): Promise<void> {
    const attempt = async () => {
      const doc = { ...this.doc, ...this.state.settings, ...patch };
      const { revision } = await this.docs.save(this.owner, doc, this.state.revision);
      this.doc = doc;
      this.set({ settings: parseOwnerSettings(doc), revision, error: null });
    };
    try {
      await attempt();
    } catch (e) {
      if (!(e instanceof SettingsConflict)) throw e;
      this.take(e.current.settings === null ? null : { doc: e.current.settings, revision: e.current.revision });
      await attempt();
    }
  }
}

const stores = new WeakMap<Docs, Map<string, Store>>();

function storeFor(docs: Docs, owner: string): Store {
  let byOwner = stores.get(docs);
  if (!byOwner) stores.set(docs, (byOwner = new Map()));
  let s = byOwner.get(owner);
  if (!s) byOwner.set(owner, (s = new Store(docs, owner)));
  return s;
}

const IDLE: OwnerSettingsState = { settings: DEFAULT_OWNER_SETTINGS, revision: 0, loaded: false, error: null };
const noop = () => () => {};

/**
 * The owner's settings, loaded once per owner and again when another device saves them (`announced`
 * is `AgentsData.settings?.revision`). Without services (signed out), the defaults.
 */
export function useOwnerSettings(docs: Docs | undefined, owner: string, announced = 0): OwnerSettingsState & { save: (patch: Partial<OwnerSettings>) => Promise<void> } {
  const store = docs ? storeFor(docs, owner) : null;
  const state = useSyncExternalStore(store?.subscribe ?? noop, store?.get ?? (() => IDLE), () => IDLE);
  useEffect(() => {
    if (store && (!store.state.loaded || announced > store.state.revision)) void store.load();
  }, [store, announced]);
  const save = useCallback(async (patch: Partial<OwnerSettings>) => {
    if (!store) throw new Error("not signed in");
    await store.save(patch);
  }, [store]);
  return { ...state, save };
}
