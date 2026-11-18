/**
 * Typed keys as you type (docs/secrets.md, "Required keys"): the server's rules
 * (`crates/roll-secrets/src/required.rs`) with the same messages, so a form says what's wrong
 * before the 400 does. The server stays the judge: a pattern JavaScript can't compile is left to it.
 */
import type { KeyType, PasswordClass, PasswordPolicy, Required } from "./secrets-api";
import { SYMBOLS } from "./secrets-generate";

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
  return rule.password ? breaks(rule.key, rule.password, value) : null;
}

const CLASS_NAME: Record<PasswordClass, string> = { lower: "a lowercase letter", upper: "an uppercase letter", digit: "a digit", symbol: "a symbol" };
const ORDER: PasswordClass[] = ["lower", "upper", "digit", "symbol"];

/** Why `value` breaks a password policy, with the server's messages (`required.rs`, `Policy::misfit`). */
function breaks(key: string, p: PasswordPolicy, value: string): string | null {
  const chars = [...value];
  if (chars.length < p.length) return `${key} must be at least ${p.length} characters`;
  const classes = ORDER.filter((c) => (p.classes?.length ? p.classes : ["lower", "upper", "digit"]).includes(c));
  const of = (ch: string): PasswordClass | null =>
    /[a-z]/.test(ch) ? "lower" : /[A-Z]/.test(ch) ? "upper" : /[0-9]/.test(ch) ? "digit" : (p.symbols ?? SYMBOLS).includes(ch) ? "symbol" : null;
  const seen = new Set<PasswordClass>();
  for (const ch of chars) {
    const c = of(ch);
    if (!c || !classes.includes(c)) return `${key} may only use ${classes.map((c) => CLASS_NAME[c]).join(", ")}`;
    seen.add(c);
  }
  const missing = classes.find((c) => !seen.has(c));
  return missing ? `${key} needs at least ${CLASS_NAME[missing]}` : null;
}
