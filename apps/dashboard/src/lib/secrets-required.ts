/**
 * Typed keys as you type (docs/secrets.md, "Required keys"): the server's rules
 * (`crates/roll-secrets/src/required.rs`) with the same messages, so a form says what's wrong
 * before the 400 does. The server stays the judge: a pattern JavaScript can't compile is left to it.
 */
import type { KeyType, Required } from "./secrets-api";

const NAME: Record<KeyType, string> = { url: "a URL", int: "an integer", port: "a port (1–65535)", bool: "a boolean", json: "JSON", email: "an email address" };

function fits(t: KeyType, v: string): boolean {
  switch (t) {
    case "url": {
      const at = v.indexOf("://");
      if (at < 1 || /\s/.test(v)) return false;
      const rest = v.slice(at + 3);
      return /^[A-Za-z][A-Za-z0-9+.-]*$/.test(v.slice(0, at)) && rest !== "" && !rest.startsWith("/");
    }
    case "int":
      return /^[+-]?\d+$/.test(v) && Number.isSafeInteger(Number(v));
    case "port":
      return /^\+?\d+$/.test(v) && Number(v) >= 1 && Number(v) <= 65535;
    case "bool":
      return ["true", "false", "1", "0", "yes", "no"].includes(v.toLowerCase());
    case "json":
      try {
        JSON.parse(v);
        return true;
      } catch {
        return false;
      }
    case "email": {
      const at = v.indexOf("@");
      const d = v.slice(at + 1);
      return at > 0 && d.includes(".") && !d.startsWith(".") && !d.endsWith(".") && !d.includes("@") && !/\s/.test(v);
    }
  }
}

/** Whether a value has a reference (`${KEY}`; `$${` is a literal): those are checked once resolved. */
export const hasReference = (v: string) => /(^|[^$])\$\{[^}]+\}/.test(v);

/** Why `value` breaks `rule` (`PORT must be a port (1–65535)`), or null. */
export function misfit(rule: Required | undefined, value: string): string | null {
  if (!rule || hasReference(value)) return null;
  if (rule.type && !fits(rule.type, value)) return `${rule.key} must be ${NAME[rule.type]}`;
  if (rule.pattern) {
    let re: RegExp;
    try {
      re = new RegExp(`^(?:${rule.pattern})$`, "u");
    } catch {
      return null;
    }
    if (!re.test(value)) return `${rule.key} must match ${rule.pattern}`;
  }
  return null;
}
