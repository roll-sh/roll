/**
 * Keeps `roll agent up` running: restarts it with exponential backoff when it exits, writes its
 * output to a log file, and stops it cleanly (SIGTERM, then SIGKILL after a grace period).
 * Modelled on T3 Code's `apps/desktop/src/backend/DesktopBackendManager.ts` (restart delay
 * doubling to a cap, terminate grace), without its readiness probing: the daemon connects out to
 * the relay on its own and reports through the canvas.
 */
import { spawn as nodeSpawn, type ChildProcess, type SpawnOptions } from "node:child_process";
import { closeSync, mkdirSync, openSync, renameSync, statSync, writeSync } from "node:fs";
import { dirname } from "node:path";

export type DaemonState =
  | { kind: "stopped" }
  | { kind: "running"; pid: number; since: number }
  | { kind: "restarting"; attempt: number; at: number; lastExit: string }
  | { kind: "stopping" };

export interface Backoff {
  initialMs: number;
  maxMs: number;
  /** A child that ran at least this long resets the attempt count. */
  resetAfterMs: number;
}

export const DEFAULT_BACKOFF: Backoff = { initialMs: 1000, maxMs: 30_000, resetAfterMs: 60_000 };

/** Delay before restart number `attempt` (0-based): initial, doubling, capped. */
export function restartDelay(b: Backoff, attempt: number): number {
  return Math.min(b.initialMs * 2 ** Math.min(attempt, 30), b.maxMs);
}

export type Spawn = (command: string, args: readonly string[], options: SpawnOptions) => ChildProcess;

export interface SupervisorOptions {
  command: string;
  args: readonly string[];
  env?: NodeJS.ProcessEnv;
  log: Log;
  backoff?: Backoff;
  stopGraceMs?: number;
  spawn?: Spawn;
  now?: () => number;
  onState?: (state: DaemonState) => void;
}

export class Supervisor {
  #o: Required<Omit<SupervisorOptions, "env" | "onState">> & Pick<SupervisorOptions, "env" | "onState">;
  #state: DaemonState = { kind: "stopped" };
  #child: ChildProcess | null = null;
  #wanted = false;
  #attempt = 0;
  #timer: ReturnType<typeof setTimeout> | null = null;
  #exited: Promise<void> = Promise.resolve();

  constructor(options: SupervisorOptions) {
    this.#o = {
      backoff: DEFAULT_BACKOFF,
      stopGraceMs: 5000,
      spawn: nodeSpawn,
      now: Date.now,
      ...options,
    };
  }

  get state(): DaemonState {
    return this.#state;
  }

  /** Starts the daemon (idempotent). */
  start(): void {
    this.#wanted = true;
    if (this.#child || this.#timer) return;
    this.#attempt = 0;
    this.#spawn();
  }

  /** Stops the daemon and any pending restart; resolves once the process has exited. */
  async stop(): Promise<void> {
    this.#wanted = false;
    if (this.#timer) {
      clearTimeout(this.#timer);
      this.#timer = null;
      this.#set({ kind: "stopped" });
    }
    const child = this.#child;
    if (!child) return;
    this.#set({ kind: "stopping" });
    this.#o.log.event(`stopping pid ${child.pid}`);
    child.kill("SIGTERM");
    const kill = setTimeout(() => {
      this.#o.log.event(`pid ${child.pid} still running after ${this.#o.stopGraceMs} ms, killing`);
      child.kill("SIGKILL");
    }, this.#o.stopGraceMs);
    await this.#exited;
    clearTimeout(kill);
  }

  #set(state: DaemonState): void {
    this.#state = state;
    this.#o.onState?.(state);
  }

  #spawn(): void {
    const { command, args, env, log } = this.#o;
    log.event(`starting ${command} ${args.join(" ")}`);
    let child: ChildProcess;
    try {
      child = this.#o.spawn(command, args, { env, stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
    } catch (e) {
      this.#exit(`failed to start: ${(e as Error).message}`, 0);
      return;
    }
    this.#child = child;
    const started = this.#o.now();
    let done = false;
    let resolveExited!: () => void;
    this.#exited = new Promise((r) => (resolveExited = r));
    const finish = (why: string) => {
      if (done) return;
      done = true;
      this.#child = null;
      resolveExited();
      this.#exit(why, this.#o.now() - started);
    };
    child.stdout?.on("data", (b: Buffer) => log.output(b));
    child.stderr?.on("data", (b: Buffer) => log.output(b));
    child.on("error", (e) => finish(`failed: ${e.message}`));
    child.on("exit", (code, signal) => finish(signal ? `killed by ${signal}` : `exited with code ${code}`));
    if (child.pid !== undefined) this.#set({ kind: "running", pid: child.pid, since: started });
  }

  #exit(why: string, ranMs: number): void {
    this.#o.log.event(why);
    if (!this.#wanted) {
      this.#set({ kind: "stopped" });
      return;
    }
    if (ranMs >= this.#o.backoff.resetAfterMs) this.#attempt = 0;
    const delay = restartDelay(this.#o.backoff, this.#attempt);
    this.#attempt += 1;
    this.#o.log.event(`restarting in ${delay} ms (attempt ${this.#attempt})`);
    this.#set({ kind: "restarting", attempt: this.#attempt, at: this.#o.now() + delay, lastExit: why });
    this.#timer = setTimeout(() => {
      this.#timer = null;
      if (this.#wanted) this.#spawn();
    }, delay);
  }
}

/** Where a supervised process's output goes. */
export interface Log {
  /** A line from the shell itself (timestamped). */
  event(line: string): void;
  /** Raw output from the process. */
  output(chunk: Buffer | string): void;
}

/**
 * An append-only log file that rotates to `<file>.1` once it passes `maxBytes`, so a chatty or
 * crash-looping daemon can't fill the disk.
 */
export class FileLog implements Log {
  readonly file: string;
  readonly name: string;
  readonly maxBytes: number;
  #fd: number | null = null;
  #size = 0;

  constructor(file: string, name: string, maxBytes = 10 * 1024 * 1024) {
    this.file = file;
    this.name = name;
    this.maxBytes = maxBytes;
  }

  event(line: string): void {
    this.#write(`[${new Date().toISOString()}] [${this.name}] ${line}\n`);
  }

  output(chunk: Buffer | string): void {
    this.#write(chunk);
  }

  close(): void {
    if (this.#fd !== null) closeSync(this.#fd);
    this.#fd = null;
  }

  #write(data: Buffer | string): void {
    try {
      if (this.#fd === null) this.#open();
      if (this.#size >= this.maxBytes) {
        this.close();
        renameSync(this.file, `${this.file}.1`);
        this.#open();
      }
      this.#size += typeof data === "string" ? writeSync(this.#fd!, data) : writeSync(this.#fd!, data);
    } catch {
      // Logging must never take the app down.
    }
  }

  #open(): void {
    mkdirSync(dirname(this.file), { recursive: true });
    this.#fd = openSync(this.file, "a", 0o600);
    try {
      this.#size = statSync(this.file).size;
    } catch {
      this.#size = 0;
    }
  }
}
