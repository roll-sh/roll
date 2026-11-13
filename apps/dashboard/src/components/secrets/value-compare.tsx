import { Fragment } from "react";
import { diffLines, splitRows, type Side } from "@/lib/diff";
import type { Link } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { LinkChip } from "./ui";

/** One side: a value, or a link (no stored value), or neither (`empty` says why). */
export type Shown = { value: string | null; link: Link | null; empty: string };

/**
 * Two revealed values side by side, old then new. Multi-line values get a line diff; anything else
 * is shown whole. The caller holds the values only while they're shown (`useMaskAgain`).
 */
export function ValueCompare({ before, after, labels = ["before", "after"] }: { before: Shown; after: Shown; labels?: [string, string] }) {
  const same = before.value === after.value && JSON.stringify(before.link) === JSON.stringify(after.link);
  const lines = before.value !== null && after.value !== null && (before.value.includes("\n") || after.value.includes("\n"));
  return (
    <div className="fade-in mt-1.5 overflow-hidden rounded-lg border border-line-2 font-mono text-[11.5px]" data-revealed>
      <div className="grid grid-cols-2 border-b border-line-2 bg-panel-2/60 text-[10.5px] text-subtle">
        <span className="px-2 py-1">{labels[0]}</span>
        <span className="border-l border-line-2 px-2 py-1">{labels[1]}</span>
      </div>
      {lines ? (
        <div className="grid max-h-72 grid-cols-2 overflow-auto">
          {splitRows(diffLines(before.value ?? "", after.value ?? "")).map(([o, n], i) => (
            <Fragment key={i}>
              <DiffCell side={o} tone="del" />
              <DiffCell side={n} tone="add" border />
            </Fragment>
          ))}
        </div>
      ) : (
        <div className="grid grid-cols-2">
          <Whole shown={before} tone={same ? null : "del"} />
          <Whole shown={after} tone={same ? null : "add"} border />
        </div>
      )}
    </div>
  );
}

const TONE = { del: "bg-danger/[0.08] text-fg", add: "bg-ok/[0.08] text-fg" } as const;

function DiffCell({ side, tone, border = false }: { side: Side; tone: "del" | "add"; border?: boolean }) {
  const changed = side && side.kind === tone;
  return (
    <div className={`flex min-w-0 gap-2 px-2 py-px ${border ? "border-l border-line-2" : ""} ${changed ? TONE[tone] : side ? "text-muted" : "bg-fg/[0.02]"}`}>
      <span className="w-6 shrink-0 text-right text-faint tabular-nums select-none">{side ? (tone === "del" ? side.old : side.new) : ""}</span>
      <span className="min-w-0 break-all whitespace-pre-wrap">{side?.text ?? ""}</span>
    </div>
  );
}

function Whole({ shown, tone, border = false }: { shown: Shown; tone: "del" | "add" | null; border?: boolean }) {
  const { resourceName } = useSecrets();
  return (
    <div className={`min-w-0 px-2 py-1.5 ${border ? "border-l border-line-2" : ""} ${tone && shown.value !== null ? TONE[tone] : ""}`}>
      {shown.value !== null ? (
        <span className="block max-h-40 overflow-auto break-all whitespace-pre-wrap">{shown.value === "" ? <span className="text-subtle italic">empty</span> : shown.value}</span>
      ) : shown.link ? (
        <LinkChip link={shown.link} name={resourceName(shown.link)} />
      ) : (
        <span className="text-subtle italic">{shown.empty}</span>
      )}
    </div>
  );
}
