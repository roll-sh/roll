/**
 * Completing a reference while it's typed (`${{ db.DATABASE_URL }}`, filled at each deploy; Railway
 * autocompletes them too): what the text before the cursor has started, and what can finish it.
 */

/** roll's own variables every service has (another's by `service.NAME`). */
export const BUILTINS = ["PORT", "ROLL_SERVICE", "ROLL_ENVIRONMENT", "ROLL_PUBLIC_URL", "ROLL_PRIVATE_HOST", "ROLL_DEPLOYMENT"];

export interface RefCatalog {
  /** This service's own variables. */
  own: string[];
  /** The project's other services and their variables' names. */
  services: { name: string; vars: string[] }[];
}

/** The reference being typed at the end of `before` (the text before the cursor), if one is open. */
export function openRef(before: string): { start: number; partial: string } | null {
  const m = /\$\{\{\s*([\w.-]*)$/.exec(before);
  return m ? { start: m.index, partial: m[1] ?? "" } : null;
}

/** What can finish `partial`: `NAME` (own), `service.` (another service), `service.NAME`. */
export function completions(partial: string, c: RefCatalog, max = 8): string[] {
  const dot = partial.indexOf(".");
  if (dot >= 0) {
    const svc = c.services.find((s) => s.name === partial.slice(0, dot));
    if (!svc) return [];
    const want = partial.slice(dot + 1).toUpperCase();
    return [...new Set([...svc.vars, ...BUILTINS])].filter((v) => v.startsWith(want)).slice(0, max).map((v) => `${svc.name}.${v}`);
  }
  const p = partial.toLowerCase();
  const own = c.own.filter((v) => v.toLowerCase().startsWith(p));
  const svcs = c.services.filter((s) => s.name.startsWith(p)).map((s) => `${s.name}.`);
  return [...svcs, ...own].slice(0, max);
}

/** `before` with the open reference finished as `choice` (closed unless it names a service to go on). */
export function complete(before: string, choice: string): string {
  const o = openRef(before);
  if (!o) return before;
  return `${before.slice(0, o.start)}\${{ ${choice}${choice.endsWith(".") ? "" : " }}"}`;
}
