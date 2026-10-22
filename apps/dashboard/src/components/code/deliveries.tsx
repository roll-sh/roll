"use client";

import { useEffect, useState } from "react";
import type { Delivery } from "@/lib/code-api";
import { formatWait } from "@/lib/queue";
import { ago } from "../ui";

const deliveryDot: Record<Delivery["state"], string> = {
  delivered: "bg-ok",
  failed: "bg-danger",
  pending: "bg-accent animate-pulse motion-reduce:animate-none",
};

/**
 * Recent webhook deliveries (a review app's or a repo webhook's), newest first: state, response
 * status, attempts, the last error, the payload, and redelivery. `deps` reloads the list when it
 * changes; so does every redelivery.
 */
export function DeliveryLog({
  load,
  redeliver,
  deps,
  busy,
  act,
}: {
  load: () => Promise<Delivery[]>;
  redeliver: (delivery: string) => Promise<Delivery>;
  deps: string;
  busy: string | null;
  act: <T>(key: string, fn: () => Promise<T>) => Promise<T | undefined>;
}) {
  const [list, setList] = useState<Delivery[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [version, setVersion] = useState(0);
  useEffect(() => {
    let alive = true;
    load().then(
      (d) => alive && (setList(d), setError(null)),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
    // `load` is a new closure every render; `deps` names what it reads.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deps, version]);

  return (
    <div className="border-t border-line bg-panel px-4 py-3">
      {error ? (
        <p className="text-[12px] text-danger">{error}</p>
      ) : !list ? (
        <p className="text-[12px] text-subtle">
          <span className="shimmer">Loading deliveries…</span>
        </p>
      ) : list.length === 0 ? (
        <p className="text-[12px] text-subtle">No deliveries yet.</p>
      ) : (
        <ul className="space-y-1">
          {list.map((d) => (
            <li key={d.id}>
              <details className="group">
                <summary className="flex cursor-pointer list-none items-center gap-3 py-1 text-[12px] marker:hidden">
                  <span className={`inline-block size-1.5 shrink-0 rounded-full ${deliveryDot[d.state]}`} title={d.state} />
                  <span className="font-mono">{d.event}</span>
                  <span className="text-subtle">
                    {d.state}
                    {d.last_status != null && ` · ${d.last_status}`} · {d.attempts} attempt{d.attempts === 1 ? "" : "s"}
                  </span>
                  {d.last_error && <span className="min-w-0 truncate text-danger">{d.last_error}</span>}
                  <span className="ml-auto shrink-0 text-[11px] text-subtle">
                    {d.state === "pending" && d.next_at ? `retry in ${formatWait(d.next_at - Date.now() / 1000)}` : ago(d.created_at)}
                  </span>
                  <button
                    type="button"
                    onClick={async (e) => {
                      e.preventDefault();
                      await act(`redeliver-${d.id}`, () => redeliver(d.id));
                      setVersion((v) => v + 1);
                    }}
                    disabled={busy !== null}
                    className="shrink-0 text-[11px] text-subtle transition hover:text-fg disabled:opacity-40"
                  >
                    {busy === `redeliver-${d.id}` ? <span className="shimmer">Sending…</span> : "Redeliver"}
                  </button>
                </summary>
                {d.last_error && (
                  <p className="mt-1 rounded-lg border border-line bg-bg px-3 py-1.5 font-mono text-[11px] break-all text-muted">
                    {d.last_status != null && <span className="text-subtle">{d.last_status} · </span>}
                    {d.last_error}
                  </p>
                )}
                <pre className="mt-1 max-h-64 overflow-auto rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[11px] leading-5 text-muted">
                  {JSON.stringify(d.payload, null, 2)}
                </pre>
              </details>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
