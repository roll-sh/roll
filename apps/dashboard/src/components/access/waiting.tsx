import { Hand } from "lucide-react";
import { Link } from "../link";
import { usePendingCount } from "./use-access";

/**
 * In every page's top bar: how many requests wait for your approval (an `ask` action), linking to
 * them. Nothing shows while none wait.
 */
export function WaitingApprovals() {
  const n = usePendingCount();
  if (n === 0) return null;
  return (
    <Link
      href="/dashboard/access/approvals"
      className="rise flex shrink-0 items-center gap-1.5 rounded-lg border border-accent/40 px-2 py-1 text-[12px] text-accent transition-colors duration-150 hover:border-accent"
      title="Requests waiting for your approval"
    >
      <Hand className="size-3.5" />
      <span className="font-mono tabular-nums">{n}</span>
      <span className="max-sm:hidden">waiting</span>
    </Link>
  );
}
