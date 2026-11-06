"use client";

/**
 * Sharing a thread (docs/code/agents.md, "Shares"): the Share button in a session's header makes a
 * read-only link to a frozen copy of the thread, and `/dashboard/agents/shares` lists an owner's
 * links and revokes them.
 */

import { ExternalLink, Link2, Share2, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { agentsApi, type ShareSummary, type Thread } from "@/lib/agents-api";
import { codeApi } from "@/lib/code-api";
import { btn, CopyButton, ErrorNote, Panel, primary, useCopy } from "../ui";
import { compact } from "./shared";
import { openSettings } from "./provider-setup";
import { useAgents } from "./use-agents";

/** The public page of a share, on this web app. */
export function shareHref(id: string) {
  return `/agents/s/${encodeURIComponent(id)}`;
}

/** The share button and its popover, in an open session's header. */
export function ShareButton({ thread }: { thread: Thread }) {
  const { client, owner } = useAgents();
  const [open, setOpen] = useState(false);
  const [subtree, setSubtree] = useState(true);
  const [busy, setBusy] = useState(false);
  const [made, setMade] = useState<ShareSummary | null>(null);
  // `made` is a link made earlier (found when it opened), not one just made.
  const [earlier, setEarlier] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { copied, copy } = useCopy();
  const box = useRef<HTMLDivElement>(null);
  const delegated = [...client.data.threads.values()].some((r) => r.value.lineage?.parent === thread.id && r.value.lineage.relation === "subagent");

  useEffect(() => {
    if (!open) return;
    const away = (e: PointerEvent) => {
      if (box.current && !box.current.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        setOpen(false);
      }
    };
    document.addEventListener("pointerdown", away);
    document.addEventListener("keydown", esc, true);
    return () => {
      document.removeEventListener("pointerdown", away);
      document.removeEventListener("keydown", esc, true);
    };
  }, [open]);

  // Reopened on a shared thread: its newest link, not "Create link" again (a second copy got made).
  useEffect(() => {
    if (!open) return;
    let live = true;
    agentsApi
      .shares(owner)
      .then((all) => {
        const mine = all.filter((s) => s.thread === thread.id).sort((a, b) => b.created_at - a.created_at)[0];
        if (live && mine) {
          setMade((m) => m ?? mine);
          setEarlier(true);
        }
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [open, owner, thread.id]);

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      setMade(await agentsApi.createShare(owner, thread.id, delegated && subtree));
      setEarlier(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const url = made ? `${window.location.origin}${shareHref(made.id)}` : "";

  return (
    <div ref={box} className="relative" data-nodrag>
      <button
        onClick={() => {
          setOpen((o) => !o);
          setMade(null);
          setEarlier(false);
          setError(null);
        }}
        title="Share a read-only replay"
        aria-label="Share"
        aria-expanded={open}
        className={`grid size-7 place-items-center rounded-md transition hover:bg-panel-2 hover:text-fg ${open ? "bg-panel-2 text-fg" : ""}`}
      >
        <Share2 className="size-3.5" />
      </button>
      {open && (
        <div role="dialog" aria-label="Share this thread" className="rise z-20 space-y-3 rounded-xl border border-line bg-panel p-3.5 text-left text-[12.5px] max-sm:fixed max-sm:inset-x-3 max-sm:top-28 sm:absolute sm:top-9 sm:right-0 sm:w-80">
          <p className="font-medium text-fg">Share a read-only replay</p>
          <p className="leading-relaxed text-muted">
            Anyone with the link can replay this thread as it is now. Later work doesn&apos;t show; terminals and images stay out, and home folders read <code className="font-mono">~</code>.
          </p>
          {made ? (
            <div className="space-y-2">
              <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-1.5">
                <code className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-fg">{url}</code>
                <CopyButton id="share" text={url} copied={copied} onCopy={copy} />
                <a href={shareHref(made.id)} target="_blank" rel="noreferrer" className="p-1 text-subtle hover:text-fg" title="Open" aria-label="Open the share">
                  <ExternalLink className="size-3.5" />
                </a>
              </div>
              {earlier && (
                <p className="flex items-center gap-2 text-[11.5px] text-subtle">
                  Made {new Date(made.created_at).toLocaleDateString(undefined, { month: "short", day: "numeric" })}: later work isn&apos;t in it.
                  <button type="button" disabled={busy} onClick={() => void create()} className="text-accent hover:underline disabled:opacity-60">
                    {busy ? <span className="shimmer">Making…</span> : "Make a new one as it is now"}
                  </button>
                </p>
              )}
              <p className="text-[11.5px] text-subtle">
                {made.threads > 1 ? `${made.threads} threads · ` : ""}
                {made.runs} run{made.runs === 1 ? "" : "s"} · {compact(made.bytes)} B ·{" "}
                <button
                  type="button"
                  onClick={() => {
                    setOpen(false);
                    openSettings("owner", "Shared links");
                  }}
                  className="text-accent hover:underline"
                >
                  Manage shares
                </button>
              </p>
            </div>
          ) : (
            <>
              {delegated && (
                <label className="flex items-center gap-2 text-muted">
                  <input type="checkbox" checked={subtree} onChange={(e) => setSubtree(e.target.checked)} className="accent-[var(--accent)]" />
                  Include the subagent threads it delegated to
                </label>
              )}
              <button className={primary} disabled={busy} onClick={() => void create()}>
                {busy ? <span className="shimmer">Making the link…</span> : (
                  <>
                    <Link2 className="size-3.5" /> Create link
                  </>
                )}
              </button>
            </>
          )}
          {error && (
            <p className="text-[12px] text-danger" role="alert">
              {error}
            </p>
          )}
        </div>
      )}
    </div>
  );
}

const LAST_OWNER = "roll:agents:owner";

/** `?owner=`, else the signed-in owner, else the last one the canvas showed, else the first. */
function useShareOwner(param: string | undefined) {
  const [owner, setOwner] = useState<string | null>(param ?? null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (param) return;
    let live = true;
    (async () => {
      try {
        const info = await codeApi.info();
        let o = info.auth?.caller.kind === "owner" ? info.auth.caller.owner : null;
        if (!o) o = localStorage.getItem(LAST_OWNER);
        if (!o) o = (await codeApi.owners())[0]?.name ?? null;
        if (live) (o ? setOwner(o) : setError("No roll code owner yet."));
      } catch (e) {
        if (live) setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      live = false;
    };
  }, [param]);
  return { owner, error };
}

/** `/dashboard/agents/shares`: an owner's links, newest first, each revocable. */
export function SharesList({ owner: param }: { owner?: string }) {
  const { owner, error: ownerError } = useShareOwner(param);
  return (
    <div className="mx-auto max-w-3xl space-y-6">
      <div className="flex items-center gap-3">
        <Share2 className="size-5 text-muted" />
        <h1 className="text-2xl font-semibold tracking-[-0.045em]">Shared threads</h1>
        {owner && <span className="font-mono text-[13px] text-subtle">{owner}</span>}
      </div>
      <Panel title="Links" aside={<span className="text-[12px] text-subtle">Read-only replays, frozen when made. Revoking one removes it at once.</span>}>
        {owner && <ShareLinks owner={owner} />}
      </Panel>
      <ErrorNote error={ownerError} />
    </div>
  );
}

/**
 * An owner's share links, each to open, copy or revoke: on the shares page and in the agents
 * settings ("Shared links", where the header's ⋯ goes, so they're found without leaving the agents).
 */
export function ShareLinks({ owner, flush = false }: { owner: string; /** In a settings row (its own padding). */ flush?: boolean }) {
  const pad = flush ? "" : "px-5";
  const [shares, setShares] = useState<ShareSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<string | null>(null);
  const { copied, copy } = useCopy();

  const load = useCallback(async () => {
    if (!owner) return;
    try {
      setShares(await agentsApi.shares(owner));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [owner]);
  useEffect(() => {
    void load();
  }, [load]);

  const revoke = async (id: string) => {
    setError(null);
    setConfirm(null);
    try {
      await agentsApi.revokeShare(owner!, id);
      setShares((s) => s?.filter((x) => x.id !== id) ?? null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  const origin = typeof window === "undefined" ? "" : window.location.origin;

  return (
    <div>
        {shares === null ? (
          <p className={`${pad} py-4 text-[13px] text-subtle`}>
            <span className="shimmer">Loading…</span>
          </p>
        ) : shares.length === 0 ? (
          <p className={`${pad} py-4 text-[13px] text-muted`}>
            Nothing shared yet. Open a conversation and use <Share2 className="inline size-3.5" aria-label="Share" /> in its header.
          </p>
        ) : (
          <ul className="divide-y divide-line" aria-label="Shares">
            {shares.map((s) => (
              <li key={s.id} className={`flex items-center gap-3 ${pad} py-3 text-[13px]`}>
                <div className="min-w-0 flex-1">
                  <a href={shareHref(s.id)} target="_blank" rel="noreferrer" className="block truncate font-medium hover:underline">
                    {s.title}
                  </a>
                  <p className="mt-0.5 font-mono text-[11px] text-subtle">
                    {new Date(s.created_at).toLocaleString()} · {s.threads > 1 ? `${s.threads} threads · ` : ""}
                    {s.runs} run{s.runs === 1 ? "" : "s"} · {compact(s.bytes)} B · by {s.created_by}
                  </p>
                </div>
                <CopyButton id={s.id} text={`${origin}${shareHref(s.id)}`} copied={copied} onCopy={copy} />
                {confirm === s.id ? (
                  <span className="flex items-center gap-1">
                    <button onClick={() => void revoke(s.id)} className="rounded px-1.5 py-0.5 text-[12px] text-danger hover:bg-danger/10">
                      Revoke link
                    </button>
                    <button onClick={() => setConfirm(null)} className="p-1 text-subtle hover:text-fg" aria-label="Keep">
                      <X className="size-3.5" />
                    </button>
                  </span>
                ) : (
                  <button onClick={() => setConfirm(s.id)} className={btn} aria-label={`Revoke ${s.title}`} title="Revoke the link">
                    <Trash2 className="size-3.5" />
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
      <ErrorNote error={error} />
    </div>
  );
}
