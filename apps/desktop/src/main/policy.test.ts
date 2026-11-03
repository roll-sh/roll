import { describe, expect, it } from "vitest";
import { deepLinkFromArgv, deepLinkPath, parseDeepLink } from "./deeplink.ts";
import { decideNavigation, externalUrl, httpOrigin, isInApp, permissionAllowed, type Origins } from "./policy.ts";

const origins: Origins = { app: "https://roll.sh", extra: ["https://github.com"] };

describe("navigation policy", () => {
  it("keeps the app's own origin in the window", () => {
    expect(decideNavigation(origins, "https://roll.sh/dashboard/agents?thread=t1", false)).toEqual({ action: "allow" });
    expect(decideNavigation(origins, "https://github.com/login/oauth/authorize", false)).toEqual({ action: "allow" });
  });

  it("sends other web pages to the system browser", () => {
    for (const url of [
      "https://evil.example/",
      "http://roll.sh/dashboard", // a different scheme is a different origin
      "https://roll.sh.evil.example/",
      "https://roll.sh:8443/",
      "https://api.roll.sh/v1/code",
    ]) {
      expect(decideNavigation(origins, url, false)).toEqual({ action: "external", url: new URL(url).href });
    }
    expect(decideNavigation(origins, "mailto:hi@example.com", false).action).toBe("external");
  });

  it("never opens new windows, even on the app's origin", () => {
    expect(decideNavigation(origins, "https://roll.sh/dashboard", true)).toEqual({ action: "external", url: "https://roll.sh/dashboard" });
  });

  it("drops anything that isn't a web link", () => {
    for (const url of ["file:///etc/passwd", "javascript:alert(1)", "data:text/html,hi", "roll://agents", "smb://host/share", "not a url", "chrome://settings"]) {
      expect(decideNavigation(origins, url, false)).toEqual({ action: "deny" });
      expect(externalUrl(url)).toBeNull();
    }
  });

  it("does not treat credentials in a URL as the app", () => {
    expect(httpOrigin("https://user:pw@roll.sh/")).toBeNull();
    expect(isInApp(origins, "https://user@roll.sh/")).toBe(false);
  });
});

describe("permissions", () => {
  it("grants only notifications, clipboard writes and fullscreen, to the app's top frame", () => {
    expect(permissionAllowed(origins, "notifications", "https://roll.sh/dashboard/agents", true)).toBe(true);
    expect(permissionAllowed(origins, "clipboard-sanitized-write", "https://roll.sh/x", true)).toBe(true);
    expect(permissionAllowed(origins, "fullscreen", "https://roll.sh/x", true)).toBe(true);
  });

  it("denies everything else", () => {
    for (const p of ["media", "geolocation", "midi", "hid", "serial", "usb", "display-capture", "openExternal", "clipboard-read", "pointerLock", "unknown"]) {
      expect(permissionAllowed(origins, p, "https://roll.sh/x", true)).toBe(false);
    }
    expect(permissionAllowed(origins, "notifications", "https://github.com/", true)).toBe(false);
    expect(permissionAllowed(origins, "notifications", "https://roll.sh/x", false)).toBe(false);
    expect(permissionAllowed(origins, "notifications", "", true)).toBe(false);
  });
});

describe("deep links", () => {
  it("parses the three link shapes", () => {
    expect(parseDeepLink("roll://agents")).toEqual({ kind: "agents" });
    expect(parseDeepLink("roll://agents/")).toEqual({ kind: "agents" });
    expect(parseDeepLink("roll://agents/thread/thr_01JX9")).toEqual({ kind: "thread", thread: "thr_01JX9" });
    expect(parseDeepLink("roll://agents/connect?code=ABCD-EFGH")).toEqual({ kind: "connect", code: "ABCD-EFGH" });
  });

  it("rejects anything else", () => {
    for (const raw of [
      "roll://code/thread/t1",
      "roll://agents/thread",
      "roll://agents/thread/t1/extra",
      "roll://agents/thread/..%2F..%2Fsettings",
      "roll://agents/thread/a%20b",
      "roll://agents/connect",
      "roll://agents/connect?code=<script>",
      "roll://user:pw@agents/thread/t1",
      "https://roll.sh/agents/thread/t1",
      "roll://agents/nope",
      `roll://agents/thread/${"a".repeat(3000)}`,
      "",
    ]) {
      expect(parseDeepLink(raw), raw).toBeNull();
    }
  });

  it("maps links to dashboard paths with escaped values", () => {
    expect(deepLinkPath({ kind: "agents" })).toBe("/dashboard/agents");
    expect(deepLinkPath({ kind: "thread", thread: "t_1" })).toBe("/dashboard/agents?focus=t%3At_1");
    expect(deepLinkPath({ kind: "connect", code: "AB-CD" })).toBe("/dashboard/agents/connect?code=AB-CD");
  });

  it("finds the link in a second instance's argv", () => {
    expect(deepLinkFromArgv(["/opt/roll/roll-desktop", "--no-sandbox", "roll://agents/thread/t9"])).toEqual({ kind: "thread", thread: "t9" });
    expect(deepLinkFromArgv(["/opt/roll/roll-desktop", "roll://bad", "roll://agents"])).toEqual({ kind: "agents" });
    expect(deepLinkFromArgv(["/opt/roll/roll-desktop"])).toBeNull();
  });
});
