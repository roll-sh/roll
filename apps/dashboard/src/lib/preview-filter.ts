import type { Preview } from "./previews-api";

/** Which previews the Previews tab shows (Vercel's deployments list has the same two controls). */
export type PreviewShow = "all" | "you" | "failed" | "awake" | "asleep";

export const SHOW_LABEL: Record<PreviewShow, string> = { all: "All", you: "Needs you", failed: "Failed", awake: "Awake", asleep: "Asleep" };

/** Waiting on a person: its pages differ from main's and nobody accepted them, or it's a fork's
 * pull request nobody has approved yet. */
export function needsYou(p: Preview): boolean {
  return p.desired !== "deleted" && (p.visual_review?.state === "changed" || p.state === "awaiting_approval");
}

function shows(p: Preview, show: PreviewShow): boolean {
  switch (show) {
    case "all":
      return true;
    case "you":
      return needsYou(p);
    case "failed":
      return p.state === "failed";
    case "awake":
      return p.state === "ready" || p.state === "creating" || p.state === "updating";
    case "asleep":
      return p.state === "sleeping";
  }
}

/** A preview against the filter's text (every word must match its title, branch, slug, repo or
 * `#number`, case aside) and its state. */
export function matches(p: Preview, text: string, show: PreviewShow): boolean {
  if (!shows(p, show)) return false;
  const words = text.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return true;
  const hay = [p.title, p.branch ?? "", p.slug, p.repo, p.number != null ? `#${p.number}` : "", p.github_pr != null ? `#${p.github_pr}` : ""].join(" ").toLowerCase();
  return words.every((w) => hay.includes(w));
}

/** How many previews each state would show, for the controls' counts. */
export function counts(list: Preview[]): Record<PreviewShow, number> {
  const out = { all: 0, you: 0, failed: 0, awake: 0, asleep: 0 } as Record<PreviewShow, number>;
  for (const p of list) for (const s of Object.keys(out) as PreviewShow[]) if (shows(p, s)) out[s]++;
  return out;
}
