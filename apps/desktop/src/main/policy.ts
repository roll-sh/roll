/**
 * What the window may load, what goes to the system browser and which web permissions the page
 * gets. Ported in spirit from T3 Code's `apps/desktop/src/window/DesktopWindow.ts`
 * (`isSameOriginRendererNavigation`) and `electron/ElectronShell.ts` (`parseSafeExternalUrl`).
 */

/** The origins the window may show: the app's own plus any configured extras (sign-in). */
export interface Origins {
  app: string;
  extra: readonly string[];
}

/** `scheme://host[:port]` of an http(s) URL, or null. */
export function httpOrigin(raw: string): string | null {
  try {
    const url = new URL(raw);
    if (url.protocol !== "https:" && url.protocol !== "http:") return null;
    if (url.username || url.password) return null;
    return url.origin;
  } catch {
    return null;
  }
}

/** True when `target` may load inside the window. */
export function isInApp(origins: Origins, target: string): boolean {
  const origin = httpOrigin(target);
  return origin !== null && (origin === origins.app || origins.extra.includes(origin));
}

const EXTERNAL_PROTOCOLS = new Set(["https:", "http:", "mailto:"]);

/** The URL to hand to the system browser, or null when it must not be opened at all. */
export function externalUrl(raw: string): string | null {
  try {
    const url = new URL(raw);
    return EXTERNAL_PROTOCOLS.has(url.protocol) ? url.href : null;
  } catch {
    return null;
  }
}

export type NavigationDecision = { action: "allow" } | { action: "external"; url: string } | { action: "deny" };

/** For `will-navigate`, `will-redirect` and `setWindowOpenHandler`. New windows are never opened. */
export function decideNavigation(origins: Origins, target: string, newWindow: boolean): NavigationDecision {
  if (!newWindow && isInApp(origins, target)) return { action: "allow" };
  const url = externalUrl(target);
  return url ? { action: "external", url } : { action: "deny" };
}

/**
 * Web permissions the app's own pages get. Everything else (camera, microphone, geolocation,
 * MIDI, HID, serial, USB, screen capture, pointer lock, `openExternal`, …) is denied, and nothing
 * is granted to any other origin or to subframes.
 */
const GRANTED = new Set(["notifications", "clipboard-sanitized-write", "fullscreen"]);

export function permissionAllowed(origins: Origins, permission: string, requestingUrl: string, isMainFrame: boolean): boolean {
  return isMainFrame && GRANTED.has(permission) && httpOrigin(requestingUrl) === origins.app;
}
