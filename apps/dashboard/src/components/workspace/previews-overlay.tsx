import { Link } from "@/components/link";
import { resourceId, type Preview } from "@/lib/previews-api";
import type { PlacedResource } from "@/lib/workspace-api";
import { cardBox, resourceKey } from "@/lib/workspace-resources";
import { previewOf, STATE_LABEL, StateDot, usePreviewMark, usePreviews } from "./previews-common";
import { formatBytes } from "@/lib/roll-api";

/** Chips over one card: the newest few, and "+N" for the rest (a card is 208 px wide). */
const SHOWN = 3;
const CHIP_H = 18;
/** From the chips' bottom to the card's top: the hairline that joins them. */
const STEM = 8;

/** A chip for one preview's branch of a card's resource. */
export interface BranchChip {
  preview: Preview;
  ref: string | null;
  state: string;
  bytes: number;
}

/** Each placed data resource's preview branches, newest preview first (the list's order). */
export function branchesByCard(previews: Preview[], placed: PlacedResource[]) {
  const here = new Set(placed.map(resourceKey));
  const out = new Map<string, BranchChip[]>();
  for (const p of previews) {
    if (p.desired === "deleted") continue;
    for (const r of p.resources) {
      const key = resourceId(r);
      if (!here.has(key)) continue;
      const list = out.get(key) ?? [];
      list.push({ preview: p, ref: r.ref, state: r.state === "ready" ? p.state : r.state, bytes: r.bytes });
      out.set(key, list);
    }
  }
  return out;
}

/**
 * The canvas's "Previews" overlay: each preview's branches as small chips pinned over their
 * parent's card by a hairline, in card coordinates (a child of `<Board>`). They don't drag; a
 * press opens the preview in the Previews tab.
 */
export function PreviewsOverlay({ projectId, placed }: { projectId: string; placed: PlacedResource[] }) {
  const { list } = usePreviews(projectId);
  usePreviewMark(list);
  if (!list) return null;
  const tab = `/dashboard/projects/${encodeURIComponent(projectId)}/previews`;
  const by = branchesByCard(list, placed);
  return (
    <>
      {placed.map((r) => {
        const chips = by.get(resourceKey(r));
        if (!chips?.length) return null;
        const box = cardBox(r);
        const shown = chips.length > SHOWN ? chips.slice(0, SHOWN - 1) : chips;
        const more = chips.length - shown.length;
        return (
          <div
            key={resourceKey(r)}
            className="rise pointer-events-none absolute flex items-end gap-1"
            style={{ left: box.x + 12, top: Math.max(0, box.y - CHIP_H - STEM), width: box.w - 24, height: CHIP_H + STEM }}
            aria-label={`Preview branches of ${r.ref}`}
          >
            {shown.map((c) => (
              <Link
                key={c.preview.id}
                href={`${tab}?preview=${encodeURIComponent(c.preview.id)}`}
                title={`${c.preview.title} · ${STATE_LABEL[c.preview.state]}\n${c.ref ?? "branching…"} · ${formatBytes(c.bytes)}`}
                className="pointer-events-auto relative flex h-[18px] max-w-[88px] min-w-0 items-center gap-1 self-start rounded-full border border-line-2 bg-sheet px-1.5 font-mono text-[10.5px] leading-none text-muted shadow-card inset-shadow-highlight transition-colors hover:border-line-3 hover:text-fg"
              >
                <StateDot state={c.state} />
                <span className="truncate">{previewOf(c.preview) || c.preview.slug}</span>
                <span aria-hidden className="absolute top-full left-2.5 h-2 w-px bg-line-3" />
              </Link>
            ))}
            {more > 0 && (
              <Link
                href={tab}
                className="pointer-events-auto relative flex h-[18px] shrink-0 items-center self-start rounded-full border border-line-2 bg-sheet px-1.5 font-mono text-[10.5px] leading-none text-muted shadow-card inset-shadow-highlight hover:border-line-3 hover:text-fg"
                title={`${more} more ${more === 1 ? "preview" : "previews"}`}
              >
                +{more}
                <span aria-hidden className="absolute top-full left-2.5 h-2 w-px bg-line-3" />
              </Link>
            )}
          </div>
        );
      })}
    </>
  );
}
