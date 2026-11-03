import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

/**
 * The dashboard declares the bridge's types in `apps/dashboard/src/lib/desktop.ts` (it can't
 * import this package). Both copies must stay identical.
 */
const NAMES = ["PendingRequest", "DesktopStatus", "DesktopNotice", "DeepLink", "RollDesktop"];

function declarations(file: string): Map<string, string> {
  const src = readFileSync(new URL(file, import.meta.url), "utf8")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\/\/.*$/gm, "");
  const out = new Map<string, string>();
  for (const name of NAMES) {
    const m = new RegExp(`export (interface ${name} \\{[\\s\\S]*?\\n\\}|type ${name} =[\\s\\S]*?;\\n)`).exec(src);
    if (m) out.set(name, m[1].replace(/\s+/g, " ").trim());
  }
  return out;
}

describe("bridge types", () => {
  it("match the dashboard's copy", () => {
    const desktop = declarations("../shared/bridge.ts");
    const dashboard = declarations("../../../dashboard/src/lib/desktop.ts");
    expect([...desktop.keys()]).toEqual(NAMES);
    expect(Object.fromEntries(dashboard)).toEqual(Object.fromEntries(desktop));
  });
});
