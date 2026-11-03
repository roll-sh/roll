import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { agentConfigPath, extractPath, findRollBinary, mergePath, readAgentLogin } from "./local.ts";
import { DEFAULT_URL, defaults, loadSettings, parseSettings, resolveSettings } from "./settings.ts";
import { fitBounds, parseState, readState, writeState } from "./state.ts";

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "roll-desktop-settings-"));
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

describe("settings", () => {
  it("writes the defaults on first run and points at roll.sh", () => {
    const file = join(dir, "settings.json");
    const s = loadSettings(file, {});
    expect(s.url).toBe(DEFAULT_URL);
    expect(s.origin).toBe("https://roll.sh");
    expect(s.apiUrl).toBe("https://api.roll.sh");
    expect(s.daemon).toBe(true);
    expect(s.autoUpdate.enabled).toBe(false);
    expect(JSON.parse(readFileSync(file, "utf8"))).toEqual(defaults);
    expect(statSync(file).mode & 0o777).toBe(0o600);
    expect(s.warnings).toEqual([]);
  });

  it("reads a self-hosted server and derives the API from its origin", () => {
    const file = join(dir, "settings.json");
    writeFileSync(file, JSON.stringify({ url: "https://code.example.com/dashboard/agents", closeToTray: false }));
    const s = loadSettings(file, {});
    expect(s.origin).toBe("https://code.example.com");
    expect(s.apiUrl).toBe("https://code.example.com");
    expect(s.closeToTray).toBe(false);
  });

  it("lets ROLL_DESKTOP_URL and ROLL_DESKTOP_API override the file for development", () => {
    const s = resolveSettings(parseSettings({ api: "https://api.example.com/" }), {
      ROLL_DESKTOP_URL: "http://ai-server.example:3000/dashboard/agents",
      ROLL_DESKTOP_API: "http://ai-server.example:7070/",
    });
    expect(s.url).toBe("http://ai-server.example:3000/dashboard/agents");
    expect(s.origin).toBe("http://ai-server.example:3000");
    expect(s.apiUrl).toBe("http://ai-server.example:7070");
  });

  it("ignores bad fields with a warning instead of failing", () => {
    const warnings: string[] = [];
    const s = parseSettings(
      { url: "file:///etc/passwd", api: 7, extraOrigins: ["https://accounts.example.com/x", "javascript:1"], daemon: "yes", autoUpdate: { enabled: true, url: "ftp://x" } },
      warnings,
    );
    expect(s.url).toBe(DEFAULT_URL);
    expect(s.api).toBeNull();
    expect(s.extraOrigins).toEqual(["https://accounts.example.com"]);
    expect(s.daemon).toBe(true);
    expect(s.autoUpdate).toEqual({ enabled: true, url: null });
    expect(warnings).toHaveLength(5);
    const env = resolveSettings(s, { ROLL_DESKTOP_URL: "javascript:alert(1)" }, warnings);
    expect(env.url).toBe(DEFAULT_URL);
  });

  it("survives a corrupt file", () => {
    const file = join(dir, "settings.json");
    writeFileSync(file, "{ nope");
    const s = loadSettings(file, {});
    expect(s.url).toBe(DEFAULT_URL);
    expect(s.warnings[0]).toMatch(/not valid JSON/);
    expect(readFileSync(file, "utf8")).toBe("{ nope"); // never overwritten
  });
});

describe("window state", () => {
  const display = { x: 0, y: 0, width: 1920, height: 1080 };

  it("round-trips bounds and the paused flag", () => {
    const file = join(dir, "state.json");
    expect(readState(file)).toEqual({ bounds: null, maximized: false, paused: false });
    writeState(file, { bounds: { x: 10, y: 20, width: 1000, height: 700 }, maximized: true, paused: true });
    expect(readState(file)).toEqual({ bounds: { x: 10, y: 20, width: 1000, height: 700 }, maximized: true, paused: true });
    expect(parseState({ bounds: { x: "1" } })).toEqual({ bounds: null, maximized: false, paused: false });
  });

  it("restores bounds that are on a display, clamped to the minimum size", () => {
    expect(fitBounds({ x: 100, y: 100, width: 1200, height: 800 }, [display])).toEqual({ x: 100, y: 100, width: 1200, height: 800 });
    expect(fitBounds({ x: 100, y: 100, width: 200, height: 100 }, [display])).toEqual({ x: 100, y: 100, width: 720, height: 480 });
  });

  it("drops bounds left on an unplugged monitor or with the title bar off screen", () => {
    expect(fitBounds({ x: 2500, y: 100, width: 1200, height: 800 }, [display])).toBeNull();
    expect(fitBounds({ x: 100, y: -500, width: 1200, height: 800 }, [display])).toBeNull();
    expect(fitBounds({ x: 2500, y: 100, width: 1200, height: 800 }, [display, { x: 1920, y: 0, width: 2560, height: 1440 }])).not.toBeNull();
    expect(fitBounds(null, [display])).toBeNull();
  });
});

describe("this machine", () => {
  it("finds the CLI: ROLL_BIN, then the bundled or workspace binary", () => {
    const has = (paths: string[]) => (p: string) => paths.includes(p);
    const base = { packaged: false, resourcesPath: "/app/resources", appPath: "/src/roll/apps/desktop", platform: "linux" as const };
    expect(findRollBinary({ ...base, env: { ROLL_BIN: "/x/roll" }, exists: has([]) })).toBe("/x/roll");
    expect(findRollBinary({ ...base, env: {}, exists: has(["/src/roll/target/debug/roll"]) })).toBe("/src/roll/target/debug/roll");
    expect(findRollBinary({ ...base, env: {}, exists: has(["/src/roll/target/debug/roll", "/src/roll/target/release/roll"]) })).toBe("/src/roll/target/release/roll");
    expect(findRollBinary({ ...base, packaged: true, env: {}, exists: has(["/app/resources/bin/roll"]) })).toBe("/app/resources/bin/roll");
    expect(findRollBinary({ ...base, packaged: true, platform: "win32", env: {}, exists: has(["/app/resources/bin/roll.exe"]) })).toBe("/app/resources/bin/roll.exe");
    expect(findRollBinary({ ...base, env: {}, exists: has([]) })).toBeNull();
  });

  it("resolves agent.json like the CLI", () => {
    expect(agentConfigPath({}, "/home/ada")).toBe("/home/ada/.config/roll/agent.json");
    expect(agentConfigPath({ XDG_CONFIG_HOME: "/cfg" }, "/home/ada")).toBe("/cfg/roll/agent.json");
    expect(agentConfigPath({ XDG_CONFIG_HOME: "", ROLL_AGENT_CONFIG: "" }, "/home/ada")).toBe("/home/ada/.config/roll/agent.json");
    expect(agentConfigPath({ ROLL_AGENT_CONFIG: "/etc/roll/agent.json", XDG_CONFIG_HOME: "/cfg" }, "/home/ada")).toBe("/etc/roll/agent.json");
  });

  it("reads whether the machine is signed in, never needing the token", () => {
    const file = join(dir, "agent.json");
    expect(readAgentLogin(file)).toBeNull();
    writeFileSync(file, JSON.stringify({ machine: "m_1", token: "rlm_x" }));
    expect(readAgentLogin(file)).toEqual({ machine: "m_1" });
    writeFileSync(file, "garbage");
    expect(readAgentLogin(file)).toEqual({ machine: null });
  });

  it("takes PATH from a login shell's noisy output and merges it", () => {
    expect(extractPath("welcome!\n__ROLL_PATH_START__/opt/homebrew/bin:/usr/bin__ROLL_PATH_END__\nbye")).toBe("/opt/homebrew/bin:/usr/bin");
    expect(extractPath("no markers")).toBeNull();
    expect(mergePath("/opt/homebrew/bin:/usr/bin", "/usr/bin:/bin")).toBe("/opt/homebrew/bin:/usr/bin:/bin");
    expect(mergePath("/a", undefined)).toBe("/a");
  });
});
