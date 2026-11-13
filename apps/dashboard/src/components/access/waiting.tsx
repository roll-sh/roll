import { Hand } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { accessApi, approvalWhere, productName, RISKY, type Approval } from "@/lib/access-api";
import { Link } from "../link";
import { usePending } from "./use-access";

/**
 * In every page's top bar: how many requests wait for your approval (an `ask` action); clicking it
 * opens them, to approve or deny right there. Nothing shows while none wait. A new one while the page is in the background notifies the
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
  const [open, setOpen] = useState(false);
  const [decided, setDecided] = useState<Set<string>>(new Set());
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const away = (e: MouseEvent) => box.current && !box.current.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("mousedown", away);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);
  const waiting = pending.filter((a) => !decided.has(a.id));
  if (waiting.length === 0) return null;
  return (
    <div ref={box} className="relative shrink-0">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className="rise flex items-center gap-1.5 rounded-lg border border-accent/40 px-2 py-1 text-[12px] text-accent transition-colors duration-150 hover:border-accent"
        title="Requests waiting for your approval"
      >
        <Hand className="size-3.5" />
        <span className="font-mono tabular-nums">{waiting.length}</span>
        <span className="max-sm:hidden">waiting</span>
      </button>
      <div
        role="dialog"
        aria-label="Requests waiting for your approval"
        className={`absolute top-full right-0 z-40 mt-1.5 w-[340px] max-w-[calc(100vw-2rem)] origin-top-right rounded-xl border border-line-2 bg-popover shadow-pop inset-shadow-highlight transition-[opacity,transform] duration-150 ease-out-soft motion-reduce:transition-none ${
          open ? "scale-100 opacity-100" : "pointer-events-none scale-95 opacity-0"
        }`}
      >
        <ul className="max-h-[60vh] divide-y divide-line overflow-y-auto">
          {waiting.slice(0, 6).map((a) => (
            <Waiting key={a.id} a={a} onDecided={() => setDecided((d) => new Set(d).add(a.id))} />
          ))}
        </ul>
        <Link href="/dashboard/access/approvals" onClick={() => setOpen(false)} className="block border-t border-line px-3 py-2 text-[12px] text-accent hover:underline">
          {waiting.length > 6 ? `All ${waiting.length} waiting` : "Open approvals"}: details, and approve for a while
        </Link>
      </div>
    </div>
  );
}

/** One waiting request: who wants to do what, approve or deny it once. */
function Waiting({ a, onDecided }: { a: Approval; onDecided: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const decide = async (approve: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await (approve ? accessApi.approve(a.id) : accessApi.deny(a.id));
      onDecided();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  const risky = RISKY.includes(a.action);
  // A request that carries a body (a bulk, a push's refs, a copy) is reviewed on its page first.
  const review = a.args !== null && a.args !== undefined;
  const where = approvalWhere(a);
  return (
    <li className="space-y-1.5 px-3 py-2.5 text-[12px]">
      <p className="text-muted">
        <b className="font-medium text-fg">{a.grant_name}</b> wants to <b className={`font-medium ${risky ? "text-danger" : "text-fg"}`}>{a.action}</b> {productName(a.product)}
      </p>
      <p className="truncate font-mono text-[11px] text-subtle">
        {a.resource ?? `${a.method} ${a.route}`}
        {where && ` · ${where}`}
      </p>
      <div className="flex items-center gap-1.5">
        {review ? (
          <Link href={`/dashboard/access/approvals?id=${encodeURIComponent(a.id)}`} className="rounded-md border border-fg/30 px-2 py-0.5 text-fg hover:border-fg/60">
            Review
          </Link>
        ) : (
          <button
            type="button"
            disabled={busy}
            onClick={() => void decide(true)}
            className={`rounded-md border px-2 py-0.5 transition-colors duration-150 ${risky ? "border-danger/50 text-danger hover:border-danger" : "border-fg/30 text-fg hover:border-fg/60"}`}
          >
            Approve
          </button>
        )}
        <button type="button" disabled={busy} onClick={() => void decide(false)} className="rounded-md border border-line px-2 py-0.5 text-muted hover:border-line-2 hover:text-fg">
          Deny
        </button>
        {error && <span className="truncate text-danger">{error}</span>}
      </div>
    </li>
  );
}
