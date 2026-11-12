/**
 * `settings.json` in the app's data directory (written with the defaults on first run), plus the
 * environment overrides for development: `ROLL_DESKTOP_URL` (the web app to load) and
 * `ROLL_DESKTOP_API` (the API `roll agent` talks to).
 */
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { httpOrigin } from "./policy.ts";

export const DEFAULT_URL = "https://roll.sh/dashboard";
/** The default before the canvas got its own window; first runs wrote it into settings.json. */
const OLD_DEFAULT_URL = "https://roll.sh/dashboard/agents";

export interface Settings {
  /**
   * The page the main window opens. Its origin is the only one the windows may show (plus
   * `extraOrigins`); the Agents window opens `<origin>/dashboard/agents`.
   */
  url: string;
  /** The API base for `roll agent login/up --api`. Unset: api.roll.sh for roll.sh, else the URL's origin. */
  api: string | null;
  /** More origins allowed in the window (a sign-in provider, say). */
  extraOrigins: string[];
  /** Run and supervise `roll agent up` (off if this machine runs the daemon some other way). */
  daemon: boolean;
  /** Closing the window keeps roll (and the agents) running in the tray. */
  closeToTray: boolean;
  /** electron-updater against a generic feed. Off until there is an update server. */
  autoUpdate: { enabled: boolean; url: string | null };
}

export const defaults: Settings = {
  url: DEFAULT_URL,
  api: null,
  extraOrigins: [],
  daemon: true,
  closeToTray: true,
  autoUpdate: { enabled: false, url: null },
};

export interface Resolved extends Settings {
  /** `api`, else `defaultApi(origin)`. */
  apiUrl: string;
  /** The URL's origin. */
  origin: string;
  /** Fields that were ignored, for the log. */
  warnings: string[];
}

type Json = Record<string, unknown>;

const isObject = (v: unknown): v is Json => typeof v === "object" && v !== null && !Array.isArray(v);

/** Parses a settings file's contents. Bad fields fall back to their defaults with a warning. */
export function parseSettings(raw: unknown, warnings: string[] = []): Settings {
  if (raw === undefined) return structuredClone(defaults);
  if (!isObject(raw)) {
    warnings.push("settings: not a JSON object, using defaults");
    return structuredClone(defaults);
  }
  const s = structuredClone(defaults);
  const url = raw.url;
  if (url !== undefined) {
    if (typeof url === "string" && httpOrigin(url)) s.url = url === OLD_DEFAULT_URL ? DEFAULT_URL : url;
    else warnings.push("settings.url: not an http(s) URL");
  }
  const api = raw.api;
  if (api !== undefined && api !== null) {
    if (typeof api === "string" && httpOrigin(api)) s.api = api.replace(/\/+$/, "");
    else warnings.push("settings.api: not an http(s) URL");
  }
  if (raw.extraOrigins !== undefined) {
    if (Array.isArray(raw.extraOrigins)) {
      for (const o of raw.extraOrigins) {
        const origin = typeof o === "string" ? httpOrigin(o) : null;
        if (origin) s.extraOrigins.push(origin);
        else warnings.push(`settings.extraOrigins: ignored ${JSON.stringify(o)}`);
      }
    } else warnings.push("settings.extraOrigins: not an array");
  }
  for (const key of ["daemon", "closeToTray"] as const) {
    const v = raw[key];
    if (v === undefined) continue;
    if (typeof v === "boolean") s[key] = v;
    else warnings.push(`settings.${key}: not a boolean`);
  }
  const au = raw.autoUpdate;
  if (au !== undefined) {
    if (isObject(au)) {
      if (typeof au.enabled === "boolean") s.autoUpdate.enabled = au.enabled;
      if (typeof au.url === "string" && httpOrigin(au.url)) s.autoUpdate.url = au.url;
      else if (au.url !== undefined && au.url !== null) warnings.push("settings.autoUpdate.url: not an http(s) URL");
    } else warnings.push("settings.autoUpdate: not an object");
  }
  return s;
}

/** Applies the environment overrides and derives the origin and API URL. */
export function resolveSettings(settings: Settings, env: NodeJS.ProcessEnv, warnings: string[] = []): Resolved {
  const s = structuredClone(settings);
  const envUrl = env.ROLL_DESKTOP_URL;
  if (envUrl) {
    if (httpOrigin(envUrl)) s.url = envUrl;
    else warnings.push("ROLL_DESKTOP_URL: not an http(s) URL, ignored");
  }
  const envApi = env.ROLL_DESKTOP_API;
  if (envApi) {
    if (httpOrigin(envApi)) s.api = envApi.replace(/\/+$/, "");
    else warnings.push("ROLL_DESKTOP_API: not an http(s) URL, ignored");
  }
  const origin = httpOrigin(s.url) ?? (httpOrigin(DEFAULT_URL) as string);
  return { ...s, origin, apiUrl: s.api ?? defaultApi(origin), warnings };
}

/** roll.sh serves its API from api.roll.sh; anywhere else the API is assumed at the web origin. */
export function defaultApi(origin: string): string {
  const url = new URL(origin);
  return url.hostname === "roll.sh" ? `${url.protocol}//api.roll.sh` : origin;
}

/** Reads (or, the first time, writes) the settings file and resolves it against `env`. */
export function loadSettings(file: string, env: NodeJS.ProcessEnv): Resolved {
  const warnings: string[] = [];
  let raw: unknown;
  try {
    const text = readFileSync(file, "utf8");
    try {
      raw = JSON.parse(text);
    } catch {
      warnings.push(`settings: ${file} is not valid JSON, using defaults`);
    }
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") writeJsonAtomic(file, defaults);
    else warnings.push(`settings: cannot read ${file}: ${(e as Error).message}`);
  }
  return resolveSettings(parseSettings(raw, warnings), env, warnings);
}

/** Write-then-rename, so a crash never leaves half a file. */
export function writeJsonAtomic(file: string, value: unknown): void {
  mkdirSync(dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify(value, null, 2) + "\n", { mode: 0o600 });
  renameSync(tmp, file);
}
