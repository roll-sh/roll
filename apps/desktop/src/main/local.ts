/**
 * This machine: where the `roll` CLI is, whether it is signed in (`agent.json`), and the PATH a
 * GUI-launched app should give it.
 */
import { execFile } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

export interface BinaryLookup {
  env: NodeJS.ProcessEnv;
  packaged: boolean;
  /** `process.resourcesPath` (packaged builds put the CLI in `<resources>/bin`). */
  resourcesPath: string;
  /** The app's directory (`apps/desktop` in development). */
  appPath: string;
  platform: NodeJS.Platform;
  exists?: (path: string) => boolean;
}

/** `ROLL_BIN`, else the bundled binary (packaged) or the workspace's `target/{release,debug}/roll`. */
export function findRollBinary(l: BinaryLookup): string | null {
  const exists = l.exists ?? existsSync;
  if (l.env.ROLL_BIN) return l.env.ROLL_BIN;
  const exe = l.platform === "win32" ? "roll.exe" : "roll";
  const candidates = l.packaged
    ? [join(l.resourcesPath, "bin", exe)]
    : [resolve(l.appPath, "../../target/release", exe), resolve(l.appPath, "../../target/debug", exe)];
  return candidates.find(exists) ?? null;
}

/**
 * Where `roll agent login` keeps the machine's login, resolved exactly as
 * `crates/roll-cli/src/agent.rs` (`saved_path`) does: `ROLL_AGENT_CONFIG`, else
 * `$XDG_CONFIG_HOME/roll/agent.json`, else `~/.config/roll/agent.json`.
 */
export function agentConfigPath(env: NodeJS.ProcessEnv, home: string): string {
  if (env.ROLL_AGENT_CONFIG) return env.ROLL_AGENT_CONFIG;
  const base = env.XDG_CONFIG_HOME || join(home, ".config");
  return join(base, "roll", "agent.json");
}

export interface AgentLogin {
  /** The machine id (the file also holds the token, which the shell never reads out). */
  machine: string | null;
}

/** Null when the machine isn't signed in. */
export function readAgentLogin(file: string): AgentLogin | null {
  let text: string;
  try {
    text = readFileSync(file, "utf8");
  } catch {
    return null;
  }
  try {
    const o = JSON.parse(text) as Record<string, unknown>;
    return { machine: typeof o.machine === "string" ? o.machine : null };
  } catch {
    // The CLI owns the format; a file we can't parse still means "signed in".
    return { machine: null };
  }
}

const START = "__ROLL_PATH_START__";
const END = "__ROLL_PATH_END__";

/** The PATH between the markers in a login shell's output (rc files may print anything). */
export function extractPath(output: string): string | null {
  const s = output.lastIndexOf(START);
  if (s === -1) return null;
  const e = output.indexOf(END, s);
  if (e === -1) return null;
  const path = output.slice(s + START.length, e).trim();
  return path || null;
}

/** The login shell's entries first, then the ones the app already had, without repeats. */
export function mergePath(shell: string, current: string | undefined, delimiter = ":"): string {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const p of [...shell.split(delimiter), ...(current ?? "").split(delimiter)]) {
    if (p && !seen.has(p)) {
      seen.add(p);
      out.push(p);
    }
  }
  return out.join(delimiter);
}

/**
 * Apps started from the Finder or a desktop launcher get a minimal PATH, so `codex`, `claude`
 * and `git` from Homebrew, nvm or ~/.local wouldn't be found. Ask the user's login shell, like
 * T3 Code's `apps/desktop/src/shell/DesktopShellEnvironment.ts` does (5 s timeout).
 */
export function loginShellPath(env: NodeJS.ProcessEnv, platform: NodeJS.Platform): Promise<string | null> {
  if (platform === "win32") return Promise.resolve(null);
  const shell = env.SHELL || (platform === "darwin" ? "/bin/zsh" : "/bin/sh");
  return new Promise((done) => {
    execFile(
      shell,
      ["-ilc", `printf '\\n${START}%s${END}\\n' "$PATH"`],
      { encoding: "utf8", timeout: 5000, killSignal: "SIGKILL", maxBuffer: 1024 * 1024, env },
      (_err, stdout) => done(extractPath(stdout ?? "")),
    );
  });
}
