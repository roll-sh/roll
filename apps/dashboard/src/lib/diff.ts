/** Unified diffs as rows: what the diff view renders in unified and split mode. */

export interface Line {
  kind: "hunk" | "add" | "del" | "ctx" | "note";
  text: string;
  old: number | null;
  new: number | null;
}

/** Parse a unified diff into rows with old/new line numbers. File headers (`---`/`+++`) are dropped. */
export function parsePatch(patch: string): Line[] {
  const out: Line[] = [];
  let o = 0;
  let n = 0;
  for (const raw of patch.split("\n")) {
    if (raw.startsWith("---") || raw.startsWith("+++") || raw.startsWith("diff ") || raw.startsWith("index ")) continue;
    if (raw.startsWith("@@")) {
      const m = /@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
      if (m) {
        o = Number(m[1]);
        n = Number(m[2]);
      }
      out.push({ kind: "hunk", text: raw, old: null, new: null });
    } else if (raw.startsWith("+")) out.push({ kind: "add", text: raw.slice(1), old: null, new: n++ });
    else if (raw.startsWith("-")) out.push({ kind: "del", text: raw.slice(1), old: o++, new: null });
    else if (raw.startsWith("\\")) out.push({ kind: "note", text: raw, old: null, new: null });
    else if (raw.length || out.length) out.push({ kind: "ctx", text: raw.slice(1), old: o++, new: n++ });
  }
  while (out.length && out[out.length - 1].kind === "ctx" && out[out.length - 1].text === "") out.pop();
  return out;
}

export type Side = Line | null;

/** Pair deletions with the additions that follow them, so a split view lines changes up. */
export function splitRows(lines: Line[]): [Side, Side][] {
  const rows: [Side, Side][] = [];
  let i = 0;
  while (i < lines.length) {
    const l = lines[i];
    if (l.kind === "del" || l.kind === "add") {
      const dels: Line[] = [];
      const adds: Line[] = [];
      while (i < lines.length && lines[i].kind === "del") dels.push(lines[i++]);
      while (i < lines.length && lines[i].kind === "add") adds.push(lines[i++]);
      for (let k = 0; k < Math.max(dels.length, adds.length); k++) rows.push([dels[k] ?? null, adds[k] ?? null]);
    } else {
      rows.push([l, l]);
      i++;
    }
  }
  return rows;
}
