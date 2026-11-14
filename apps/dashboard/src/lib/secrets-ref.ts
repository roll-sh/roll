/**
 * A key's `roll://` reference (docs/secrets.md, "References in files"), as `roll run --env-file`
 * and `roll secrets render` resolve it: `roll://ENV/KEY`, or `roll://ENV/folder/KEY` in a folder.
 * It names the folder the key is shown in, so it resolves to what that folder reads (its own
 * value, an import or an inherited one). Null when a part can't be written in one (the CLI's
 * grammar: letters, digits, `-`, `_` and `.`).
 */
export function rollRef(env: string, path: string, key: string): string | null {
  const parts = [env, ...path.split("/").filter(Boolean), key];
  const ok = (p: string) => p !== "." && p !== ".." && /^[A-Za-z0-9._-]+$/.test(p);
  return parts.every(ok) ? `roll://${parts.join("/")}` : null;
}
