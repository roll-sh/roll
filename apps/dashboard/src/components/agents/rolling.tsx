"use client";

import { ChevronRight, X } from "lucide-react";
import { usePathname, useRouter } from "next/navigation";
import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { codeApi } from "@/lib/code-api";
import { subscribeToken } from "@/lib/code-token";
import { activeClient, clientFor, DEMO_OWNER, focusOnCanvas, type AgentsClient } from "@/lib/agents-client";
import { rolling } from "@/lib/agents-graph";
import { RequestCard } from "./request-card";
import { StatusDot } from "./shared";
import { useAgentsSnapshot, useHold, useMarkDriver } from "./use-agents";

/** The signed-in roll code owner, if the token is an owner's (cached per token). */
let who: { token: number; owner: Promise<string | null> } | null = null;
let tokenVersion = 0;
if (typeof window !== "undefined") subscribeToken(() => tokenVersion++);

function signedInOwner(): Promise<string | null> {
  if (!who || who.token !== tokenVersion) {
    who = {
      token: tokenVersion,
      owner: codeApi
        .info()
        .then((i) => (i.auth?.caller.kind === "owner" ? i.auth.caller.owner : null))
        .catch(() => null),
    };
  }
  return who.owner;
}

/** The client the header follows: the one the agents page shows, else the signed-in owner's. */
function useHeaderClient(): AgentsClient | null {
  const active = useSyncExternalStore(activeClient.subscribe, activeClient.get, () => null);
  const [owner, setOwner] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    void signedInOwner().then((o) => live && setOwner(o));
    return () => {
      live = false;
    };
  }, []);
  if (active) return active;
  return owner ? clientFor(owner) : null;
}

/**
 * "rolling" in the header of every dashboard page: what agents are working on and what's waiting
 * for you, answerable right here, with a jump to the node on the canvas. It also drives the
 * shared logo (thinking while anything runs, success when a run completes).
 */
export function Rolling() {
  const client = useHeaderClient();
  if (!client) return null;
  return <RollingFor client={client} />;
}

function RollingFor({ client }: { client: AgentsClient }) {
  useHold(client);
  const snap = useAgentsSnapshot(client);
  useMarkDriver(snap);
  const [open, setOpen] = useState(false);
  const panel = useRef<HTMLDivElement>(null);
  const router = useRouter();
  const pathname = usePathname();

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (panel.current && !panel.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  if (!snap) return null;
  const { busy, requests } = rolling(snap.data);
  const count = busy.length;
  const onCanvas = pathname === "/dashboard/agents";
  const demo = client.owner === DEMO_OWNER;
  const jump = (key: string) => {
    setOpen(false);
    if (onCanvas) focusOnCanvas(key);
    else router.push(`/dashboard/agents?${demo ? "demo&" : `owner=${encodeURIComponent(client.owner)}&`}focus=${encodeURIComponent(key)}`);
  };
  const title = (thread: string) => snap.data.threads.get(thread)?.value.title ?? thread;

  return (
    <div className="relative" ref={panel}>
      <button
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        aria-haspopup="dialog"
        className={`inline-flex items-center gap-1.5 rounded-lg border px-2.5 py-1.5 text-[12px] transition ${
          count || requests.length ? "border-accent/40 text-fg" : "border-line text-muted hover:border-line-2 hover:text-fg"
        }`}
        title="What your agents are doing"
      >
        <span className={`size-1.5 rounded-full ${count ? "pulse-dot bg-accent" : "bg-subtle"}`} aria-hidden />
        {count ? "rolling" : "agents"}
        {(count > 0 || requests.length > 0) && (
          <span className="font-mono tabular-nums">
            {count}
            {requests.length > 0 && <span className="text-accent"> · {requests.length}</span>}
          </span>
        )}
      </button>
      {open && (
        <div role="dialog" aria-label="Rolling" className="rise absolute top-full right-0 z-40 mt-2 w-[380px] overflow-hidden rounded-xl border border-line bg-panel">
          <div className="flex items-center justify-between border-b border-line px-4 py-2.5">
            <p className="text-[13px] font-medium">{count ? `${count} rolling` : "Nothing running"}</p>
            <button onClick={() => setOpen(false)} aria-label="Close" className="text-subtle hover:text-fg">
              <X className="size-3.5" />
            </button>
          </div>
          <div className="max-h-[70vh] overflow-y-auto">
            {requests.length > 0 && (
              <section aria-label="Waiting for you" className="border-b border-line">
                <p className="px-4 pt-3 text-[11px] tracking-wide text-subtle uppercase">Waiting for you</p>
                <ul>
                  {requests.map((q) => {
                    const machine = snap.data.threads.get(q.thread)?.machine;
                    return (
                      <li key={q.id} className="border-b border-line px-4 py-3 last:border-0">
                        <RequestCard
                          request={q}
                          compact
                          context={
                            <button onClick={() => jump(`t:${q.thread}`)} className="hover:text-fg">
                              {title(q.thread)} <ChevronRight className="inline size-3" />
                            </button>
                          }
                          onRespond={(response) => client.run(machine!, { type: "request.respond", request: q.id, response })}
                        />
                      </li>
                    );
                  })}
                </ul>
              </section>
            )}
            {busy.length > 0 ? (
              <ul aria-label="Rolling threads" className="py-1.5">
                {busy.map((t) => (
                  <li key={t.id}>
                    <button onClick={() => jump(`t:${t.id}`)} className="flex w-full items-start gap-2.5 px-4 py-2 text-left hover:bg-panel-2">
                      <StatusDot status={t.status} className="mt-[7px]" />
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-[13px]">{t.title}</span>
                        <span className="block truncate text-[12px] shimmer">{t.activity ?? "Working"}</span>
                      </span>
                      <ChevronRight className="mt-1 size-3.5 text-subtle" />
                    </button>
                  </li>
                ))}
              </ul>
            ) : (
              requests.length === 0 && <p className="px-4 py-6 text-center text-[13px] text-subtle">Your agents are idle. Keep the code rolling.</p>
            )}
          </div>
          {!onCanvas && (
            <button onClick={() => jump("")} className="block w-full border-t border-line px-4 py-2.5 text-left text-[12.5px] text-accent hover:bg-panel-2">
              Open the canvas
            </button>
          )}
        </div>
      )}
    </div>
  );
}
