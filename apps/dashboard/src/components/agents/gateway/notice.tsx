"use client";

/**
 * THE NOTICE (docs/code/gateway.md, "THE NOTICE"; the user's explicit requirement): everywhere a
 * machine uses roll's model gateway, roll says plainly that its AI requests go out from the
 * gateway's computer (its network and IP address, which providers see), and what happens when
 * that computer is offline. The words are `noticeText` in `lib/agents-gateway.ts`, written once;
 * this is the one component that shows them:
 *
 * - `panel`: setup (with `onAcknowledge`, a checkbox the owner ticks), the Gateway section's list
 *   of machines, the new-sandbox dialog, a member's endpoint;
 * - `inline`: under a provider that goes through the gateway ("via roll gateway on <host>").
 */

import { Globe } from "lucide-react";
import { useId, type ReactNode } from "react";
import { hostNoticeText, noticeText, type NoticeWho } from "@/lib/agents-gateway";
import { cn } from "../ui";

export function GatewayNotice({
  host,
  who,
  fallback,
  self = false,
  variant = "panel",
  acknowledged,
  onAcknowledge,
  className,
  children,
}: {
  /** The gateway computer's name. */
  host: string;
  who: NoticeWho;
  fallback: boolean;
  /** The machine is the gateway's computer itself. */
  self?: boolean;
  variant?: "panel" | "inline";
  /** Setup: the checkbox's state, and its change. */
  acknowledged?: boolean;
  onAcknowledge?: (on: boolean) => void;
  className?: string;
  children?: ReactNode;
}) {
  const id = useId();
  const text = self ? hostNoticeText(host) : noticeText(host, who, fallback);
  if (variant === "inline") {
    return (
      <p data-gateway-notice className={cn("flex items-start gap-1.5 text-[11.5px] leading-snug text-subtle", className)}>
        <Globe className="mt-px size-3 shrink-0" strokeWidth={1.75} aria-hidden />
        <span>{text}</span>
      </p>
    );
  }
  return (
    <div data-gateway-notice role="note" aria-labelledby={`${id}-t`} className={cn("rounded-lg border border-line-2 bg-panel-2 px-3 py-2.5 text-[12.5px] inset-shadow-highlight", className)}>
      <div className="flex items-start gap-2.5">
        <span className="mt-0.5 grid size-6 shrink-0 place-items-center rounded-md border border-line-2 bg-raised text-muted inset-shadow-highlight" aria-hidden>
          <Globe className="size-3.5" strokeWidth={1.75} />
        </span>
        <div className="min-w-0 flex-1 space-y-1.5">
          <p id={`${id}-t`} className="leading-relaxed text-soft">
            {text}
          </p>
          {children}
          {onAcknowledge && (
            <label className="flex cursor-pointer items-center gap-2 pt-0.5 text-[12.5px] font-[510] text-fg">
              <input type="checkbox" checked={!!acknowledged} onChange={(e) => onAcknowledge(e.target.checked)} className="size-3.5 accent-[var(--accent)]" />
              I understand: these requests go out from {host}.
            </label>
          )}
        </div>
      </div>
    </div>
  );
}
