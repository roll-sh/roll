/**
 * The live agents state for one owner: loads the relay's snapshot, follows its event stream with
 * `Last-Event-ID` resume, keeps the detail of the threads someone on the page follows, and runs
 * commands. React reads it through `useSyncExternalStore` (`components/agents/use-agents.ts`).
 *
 * The transport is pluggable: `httpTransport` talks to `/v1/code/agents`, and the demo
 * (`agents-demo.ts`) simulates an owner in memory behind the same interface.
 */

import {
  agentsApi,
  commandId,
  CommandError,
  expectOk,
  streamEventD,
  type AgentsApi,
  type Command,
  type MachineId,
  type Reply,
  type State,
  type StreamEvent,
  type TerminalId,
  type ThreadId,
} from "./agents-api";
import { decode } from "./decode";
import { applyEvents, applySnapshot, emptyData, follow, type AgentsData } from "./agents-store";
import { streamSse } from "./sse";

export interface AgentsTransport {
  /** The snapshot, with the detail of `threads`. */
  state(threads: readonly ThreadId[]): Promise<State>;
  /**
   * Follow events after the cursor `after()` returns (it's asked again on every reconnect).
   * Returns the function that stops it.
   */
  stream(threads: readonly ThreadId[], after: () => number, on: (ev: StreamEvent, id: number | null) => void, onOpen: (open: boolean) => void): () => void;
  command(machine: MachineId, id: string, command: Command): Promise<Reply>;
}

export function httpTransport(owner: string, api: AgentsApi = agentsApi): AgentsTransport {
  return {
    state: (threads) => api.state(owner, threads),
    stream: (threads, after, on, onOpen) =>
      streamSse(api.eventsUrl(owner, threads), {
        headers: () => {
          const c = after();
          return { ...api.authHeaders(), ...(c > 0 ? { "last-event-id": String(c) } : {}) };
        },
        onOpen,
        onEvent: (e) => {
          let ev: StreamEvent;
          try {
            ev = decode(streamEventD, JSON.parse(e.data));
          } catch {
            return; // Something this dashboard doesn't know yet.
          }
          const id = e.id !== undefined && /^\d+$/.test(e.id) ? Number(e.id) : null;
          on(ev, id);
        },
      }),
    command: (machine, id, command) => api.command(owner, machine, { id, command }),
  };
}

export type ConnState = "connecting" | "live" | "reconnecting" | "error";

export interface AgentsSnapshot {
  data: AgentsData;
  conn: ConnState;
  error: string | null;
}

/** Terminal output kept per terminal for a view that opens late (base64 chunks, ~256 KiB). */
const TERMINAL_KEEP = 256 * 1024;

export class AgentsClient {
  private snap: AgentsSnapshot = { data: emptyData(), conn: "connecting", error: null };
  private listeners = new Set<() => void>();
  private queue: StreamEvent[] = [];
  private flushing = false;
  private stop: (() => void) | null = null;
  private resume = 0;
  private holds = 0;
  private wants = new Map<string, ReadonlySet<ThreadId>>();
  private followTimer: ReturnType<typeof setTimeout> | null = null;
  private generation = 0;
  private terminals = new Map<TerminalId, { chunks: string[]; size: number; listeners: Set<(b64: string) => void> }>();

  constructor(
    readonly owner: string,
    private transport: AgentsTransport,
    /** Batches stream events into one update per frame. Tests pass a synchronous scheduler. */
    private schedule: (fn: () => void) => void = (fn) =>
      typeof requestAnimationFrame === "function" ? requestAnimationFrame(() => fn()) : setTimeout(fn, 16),
  ) {}

  // ── React ──

  subscribe = (fn: () => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  getSnapshot = () => this.snap;

  private set(patch: Partial<AgentsSnapshot>) {
    const next = { ...this.snap, ...patch };
    if (next.data === this.snap.data && next.conn === this.snap.conn && next.error === this.snap.error) return;
    this.snap = next;
    for (const l of this.listeners) l();
  }

  get data() {
    return this.snap.data;
  }

  // ── lifecycle ──

  /** Keep the connection while held; the last release closes it. */
  hold(): () => void {
    if (this.holds++ === 0) void this.connect();
    let released = false;
    return () => {
      if (released) return;
      released = true;
      if (--this.holds === 0) this.disconnect();
    };
  }

  private followed(): Set<ThreadId> {
    const all = new Set<ThreadId>();
    for (const s of this.wants.values()) for (const t of s) all.add(t);
    return all;
  }

  private async connect() {
    const gen = ++this.generation;
    const threads = this.followed();
    this.set({ conn: this.snap.conn === "live" ? "reconnecting" : this.snap.conn });
    try {
      const state = await this.transport.state([...threads].sort());
      if (gen !== this.generation) return;
      this.set({ data: applySnapshot(this.snap.data, state, threads), error: null });
      this.resume = state.cursor;
      this.openStream(threads, gen);
    } catch (e) {
      if (gen !== this.generation) return;
      this.set({ conn: "error", error: e instanceof Error ? e.message : String(e) });
      // Try again shortly; the page keeps showing the last state it had.
      setTimeout(() => {
        if (gen === this.generation && this.holds > 0) void this.connect();
      }, 4000);
    }
  }

  private openStream(threads: ReadonlySet<ThreadId>, gen: number) {
    this.stop?.();
    this.stop = this.transport.stream(
      [...threads].sort(),
      () => this.resume,
      (ev, id) => {
        if (gen !== this.generation) return;
        if (id !== null && id > this.resume) this.resume = id;
        this.enqueue(ev);
      },
      (open) => {
        if (gen !== this.generation) return;
        this.set({ conn: open ? "live" : "reconnecting" });
      },
    );
  }

  private disconnect() {
    this.generation++;
    this.stop?.();
    this.stop = null;
    if (this.followTimer) clearTimeout(this.followTimer);
    this.followTimer = null;
  }

  // ── following threads (detail scope) ──

  /** `who` follows the detail of `threads` (replacing what it followed before). */
  follow(who: string, threads: Iterable<ThreadId>) {
    const next = new Set(threads);
    const prev = this.wants.get(who);
    if (prev && prev.size === next.size && [...next].every((t) => prev.has(t))) return;
    if (next.size) this.wants.set(who, next);
    else this.wants.delete(who);
    this.refollow();
  }

  unfollow(who: string) {
    if (this.wants.delete(who)) this.refollow();
  }

  private refollow() {
    if (this.followTimer) clearTimeout(this.followTimer);
    this.followTimer = setTimeout(() => {
      this.followTimer = null;
      const want = this.followed();
      const have = this.snap.data.detail;
      const added = [...want].some((t) => !have.has(t));
      const removed = [...have].some((t) => !want.has(t));
      if (!added && !removed) return;
      if (this.holds === 0) {
        this.set({ data: follow(this.snap.data, want) });
        return;
      }
      if (added) {
        // New detail needs a snapshot; then the stream resumes from it with the new set.
        void this.connect();
      } else {
        this.set({ data: follow(this.snap.data, want) });
        this.openStream(want, this.generation);
      }
    }, 120);
  }

  // ── events ──

  /** Apply events (the transport's, or a test's) in the next batch. */
  enqueue(ev: StreamEvent) {
    if (ev.type === "delta" && ev.delta.type === "terminal") {
      this.terminalBytes(ev.delta.terminal, ev.delta.data);
      return;
    }
    this.queue.push(ev);
    if (this.flushing) return;
    this.flushing = true;
    this.schedule(() => {
      this.flushing = false;
      const batch = this.queue;
      this.queue = [];
      this.set({ data: applyEvents(this.snap.data, batch) });
    });
  }

  private terminalBytes(id: TerminalId, b64: string) {
    let t = this.terminals.get(id);
    if (!t) this.terminals.set(id, (t = { chunks: [], size: 0, listeners: new Set() }));
    t.chunks.push(b64);
    t.size += b64.length;
    while (t.size > TERMINAL_KEEP && t.chunks.length > 1) t.size -= t.chunks.shift()!.length;
    for (const l of t.listeners) l(b64);
  }

  /** Terminal output as it arrives (base64). Returns what was kept so far and the unsubscribe. */
  onTerminal(id: TerminalId, fn: (b64: string) => void): { backlog: string[]; off: () => void } {
    let t = this.terminals.get(id);
    if (!t) this.terminals.set(id, (t = { chunks: [], size: 0, listeners: new Set() }));
    t.listeners.add(fn);
    const tt = t;
    return { backlog: [...t.chunks], off: () => tt.listeners.delete(fn) };
  }

  // ── commands ──

  /** Send a command; one id per call (retries inside reuse it). Resolves with the daemon's reply. */
  send(machine: MachineId, command: Command, id = commandId()): Promise<Reply> {
    return this.transport.command(machine, id, command);
  }

  /** `send`, throwing a `CommandError` for an error reply. */
  async run<R extends Reply = Reply>(machine: MachineId, command: Command, ...types: R["type"][]): Promise<R> {
    return expectOk<R>(await this.send(machine, command), ...types);
  }
}

export { CommandError };

// ── one client per owner, shared by the canvas and the header popover ──

const clients = new Map<string, AgentsClient>();

export function clientFor(owner: string, make: () => AgentsTransport = () => httpTransport(owner)): AgentsClient {
  let c = clients.get(owner);
  if (!c) clients.set(owner, (c = new AgentsClient(owner, make())));
  return c;
}

/** The client the agents page is showing (the demo, or an owner), so the header follows it. */
let active: AgentsClient | null = null;
const activeListeners = new Set<() => void>();

export function setActiveClient(c: AgentsClient | null) {
  if (active === c) return;
  active = c;
  for (const l of activeListeners) l();
}

export const activeClient = {
  subscribe: (fn: () => void) => {
    activeListeners.add(fn);
    return () => activeListeners.delete(fn);
  },
  get: () => active,
};
