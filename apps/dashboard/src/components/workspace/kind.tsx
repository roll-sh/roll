import { Boxes, Database, GitBranch, Package, Waves, Zap, type LucideIcon } from "lucide-react";
import type { ResourceKind, WorkspaceProject } from "@/lib/workspace-api";
import { CARD_H, CARD_W } from "@/lib/workspace-resources";

export const KIND_ICON: Record<ResourceKind, LucideIcon> = {
  postgres: Database,
  redis: Zap,
  nosql: Boxes,
  stream: Waves,
  bucket: Package,
  repo: GitBranch,
};

/** A resource kind's icon on a raised tile. */
export function KindTile({ kind, size = 24 }: { kind: ResourceKind; size?: number }) {
  const Icon = KIND_ICON[kind];
  return (
    <span
      className="grid shrink-0 place-items-center rounded-md border border-line-2 bg-raised text-fg inset-shadow-highlight"
      style={{ width: size, height: size }}
    >
      <Icon className="size-3.5" />
    </span>
  );
}

/**
 * A project's tile: its own canvas in miniature (each card a bar where it sits), so projects are
 * told apart by their shape. Staged cards are the accent.
 */
export function MiniMap({
  project,
  staged = [],
  size = 26,
}: {
  project: Pick<WorkspaceProject, "resources">;
  staged?: { x: number; y: number }[];
  size?: number;
}) {
  const cards = [...project.resources.map((r) => ({ x: r.x, y: r.y, s: false })), ...staged.map((r) => ({ x: r.x, y: r.y, s: true }))];
  const box = "grid shrink-0 place-items-center rounded-md border border-line-2 bg-canvas";
  if (!cards.length) return <span className={box} style={{ width: size, height: size }} aria-hidden />;
  const x0 = Math.min(...cards.map((c) => c.x));
  const y0 = Math.min(...cards.map((c) => c.y));
  const x1 = Math.max(...cards.map((c) => c.x)) + CARD_W;
  const y1 = Math.max(...cards.map((c) => c.y)) + CARD_H;
  const inner = size - 9;
  const k = inner / Math.max(x1 - x0, y1 - y0);
  const ox = (size - (x1 - x0) * k) / 2;
  const oy = (size - (y1 - y0) * k) / 2;
  return (
    <span className={box} style={{ width: size, height: size }} aria-hidden>
      <svg width={size} height={size} className="block">
        {cards.map((c, i) => (
          <rect
            key={i}
            x={ox + (c.x - x0) * k}
            y={oy + (c.y - y0) * k}
            width={Math.max(2, CARD_W * k)}
            height={Math.max(1.5, CARD_H * k)}
            rx={0.8}
            className={c.s ? "fill-accent" : "fill-muted"}
          />
        ))}
      </svg>
    </span>
  );
}
