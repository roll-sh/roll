import { checkSummary, endpointSlower, type Preview } from "./previews-api";

/** A section of the preview's sheet a verdict item jumps to. */
export type VerdictTarget = "services" | "visual" | "api" | "schema" | "checks" | "endpoints";

export interface VerdictItem {
  target: VerdictTarget;
  text: string;
  /** `danger`: breaks something; `attention`: waits on someone; `note`: worth a look. */
  tone: "danger" | "attention" | "note";
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** What a preview's sheet leads with (Vercel's deployment summary, Chromatic's build status):
 * what changed against main and what waits on someone, most serious first, each pointing at its
 * section. Empty when nothing is new. Built only from the preview's record. */
export function verdict(p: Preview): VerdictItem[] {
  const out: VerdictItem[] = [];
  const failed = p.services.filter((s) => s.state === "failed").length;
  if (failed) out.push({ target: "services", text: `${plural(failed, "service")} failed`, tone: "danger" });
  const breaking = p.base_change?.api_breaking?.length ?? 0;
  if (breaking) out.push({ target: "api", text: `${plural(breaking, "breaking API change")}`, tone: "danger" });
  const risks = p.schema_risks?.length ?? 0;
  if (risks) out.push({ target: "schema", text: `${plural(risks, "migration risk")}`, tone: "danger" });
  const added = checkSummary(p).added ?? 0;
  if (added > 0) out.push({ target: "checks", text: `${plural(added, "new problem")} against ${p.base_change?.branch ?? "main"}`, tone: "danger" });
  const v = p.visual_review;
  if (v?.state === "changed") {
    // The pages nobody accepted yet (all of them when none says).
    const n = v.pages.filter((x) => !x.by).length || v.pages.length || 1;
    out.push({ target: "visual", text: `${plural(n, "page")} ${n === 1 ? "differs" : "differ"} from ${v.branch}${v.required ? ": review before it lands" : ""}`, tone: "attention" });
  }
  const slower = p.base_change?.endpoints?.filter(endpointSlower).length ?? 0;
  if (slower) out.push({ target: "endpoints", text: `${plural(slower, "endpoint")} slower than ${p.base_change?.branch ?? "main"}`, tone: "note" });
  const moving = v?.pages.filter((x) => x.unstable).length ?? 0;
  if (moving) out.push({ target: "visual", text: `${plural(moving, "page")} ${moving === 1 ? "moves" : "move"} by itself`, tone: "note" });
  return out;
}
