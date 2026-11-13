import { Boxes, Database, GitBranch, Inbox, ListChecks, Package, TextSearch, Waves, Zap, type LucideIcon } from "lucide-react";
import type { ResourceKind } from "@/lib/workspace-api";

export const KIND_ICON: Record<ResourceKind, LucideIcon> = {
  postgres: Database,
  redis: Zap,
  nosql: Boxes,
  search: TextSearch,
  stream: Waves,
  bucket: Package,
  queue: Inbox,
  tasks: ListChecks,
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
