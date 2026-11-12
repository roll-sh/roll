import { Hand } from "lucide-react";
import { useEffect, useRef } from "react";
import { productName } from "@/lib/access-api";
import { Link } from "../link";
import { usePending } from "./use-access";

/**
 * In every page's top bar: how many requests wait for your approval (an `ask` action), linking to
 * them. Nothing shows while none wait. A new one while the page is in the background notifies the
 * browser, when you've allowed notifications (the approvals page asks).
 */
export function WaitingApprovals() {
  const pending = usePending();
  const seen = useRef<Set<string> | null>(null);
  useEffect(() => {
    const ids = new Set(pending.map((a) => a.id));
    // The first look only learns what's there: nothing new yet.
    if (seen.current && typeof Notification !== "undefined" && Notification.permission === "granted" && document.visibilityState !== "visible") {
      for (const a of pending.filter((a) => !seen.current?.has(a.id))) {
        const n = new Notification(`${a.grant_name} needs your approval`, { body: `${a.action} ${productName(a.product)} ${a.resource ?? ""}`.trim(), tag: a.id });
        n.onclick = () => {
          window.focus();
          window.location.assign(`/dashboard/access/approvals?id=${encodeURIComponent(a.id)}`);
        };
      }
    }
    seen.current = ids;
  }, [pending]);
  if (pending.length === 0) return null;
  return (
    <Link
      href="/dashboard/access/approvals"
      className="rise flex shrink-0 items-center gap-1.5 rounded-lg border border-accent/40 px-2 py-1 text-[12px] text-accent transition-colors duration-150 hover:border-accent"
      title="Requests waiting for your approval"
    >
      <Hand className="size-3.5" />
      <span className="font-mono tabular-nums">{pending.length}</span>
      <span className="max-sm:hidden">waiting</span>
    </Link>
  );
}
