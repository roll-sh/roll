import type { WorkInput } from "./code-api";
import type { ErrorGroup, Preview } from "./previews-api";

type Where = Pick<Preview, "repo" | "number">;

const oneLine = (s: string) => s.replace(/[\r\n`]+/g, " ").replace(/\s+/g, " ").trim();
const cut = (s: string, n: number) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);
const from = (p: Where) => (p.repo && p.number ? ` of ${p.repo}#${p.number}` : "");

/** An error group of a preview made a work item: its service and title, its sample (masked, as the
 * preview shows it) in a block nothing in it can close, and how often it was seen. */
export function workFromError(p: Where, g: ErrorGroup): WorkInput {
  const run = Math.max(0, ...Array.from(g.sample.matchAll(/`+/g), (m) => m[0].length));
  const fence = "`".repeat(Math.max(3, run + 1));
  return {
    title: cut(`${oneLine(g.service)}: ${oneLine(g.title)}`, 100),
    body: `Seen ${g.count} time${g.count === 1 ? "" : "s"} in the preview${from(p)} (\`${oneLine(g.service)}\`), last at ${new Date(g.last_seen).toISOString()}.\n\n${fence}\n${g.sample.slice(0, 4000)}\n${fence}`,
    labels: ["preview-error"],
  };
}
