import { ArrowLeft, Columns2, History as HistoryIcon, RotateCcw, X } from "lucide-react";
import { useEffect, useState } from "react";
import { needsApproval, type Change, type Environment, type SecretType, type Version } from "@/lib/secrets-api";
import { Dialog } from "../agents/ui/dialog";
import { ItemWhat } from "./requests";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, ErrorNote, errorText, field, ghost, LinkChip, primary, Reveal, stamp, useLoad, useMaskAgain } from "./ui";
import { ValueCompare, type Shown } from "./value-compare";

export type Drawer = { kind: "changes" } | { kind: "change"; seq: number } | { kind: "versions"; key: string; type: SecretType };

const KIND_LABEL: Record<Change["kind"], string> = {
  write: "write",
  rollback: "rollback",
  promote: "promote",
  rotate: "rotate",
  request: "request",
  rename: "rename",
};

/**
 * The history sheet over the environment's right side: its changes newest first, a change's
 * detail (what each key did, by fingerprint), a rollback to any change, and one secret's versions
 * with old values revealed one at a time or next to the version before.
 */
export function HistoryDrawer({ env, drawer, onClose, onDrawer, onChanged }: { env: Environment; drawer: Drawer | null; onClose: () => void; onDrawer: (d: Drawer) => void; onChanged: () => void }) {
  const open = !!drawer;
  // Keep what it showed while it slides out.
  const [shown, setShown] = useState<Drawer | null>(drawer);
  useEffect(() => {
    if (drawer) setShown(drawer);
  }, [drawer]);
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented && !(e.target as HTMLElement | null)?.closest("[role=dialog], [role=menu]")) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);
  // In the flow beside the list on a wide screen (the list narrows as it slides in), over it on a phone.
  return (
    <div className={`shrink-0 transition-[width] duration-[260ms] ease-drawer motion-reduce:transition-none max-md:w-0 ${open ? "md:w-[416px]" : "md:w-0"}`}>
      <aside
        data-panel
        aria-hidden={!open}
        aria-label="History"
        inert={!open}
        className={`sticky top-2 flex h-[calc(100svh-7.5rem)] w-[400px] md:ml-2 flex-col overflow-hidden rounded-xl border border-line-2 bg-sheet shadow-sheet inset-shadow-highlight transition-[transform,opacity] duration-[260ms] ease-drawer motion-reduce:transition-none max-md:fixed max-md:top-24 max-md:right-2 max-md:bottom-2 max-md:z-30 max-md:h-auto max-md:w-[calc(100vw-1rem)] ${
          open ? "translate-x-0 opacity-100" : "pointer-events-none translate-x-8 opacity-0"
        }`}
      >
        {shown?.kind === "changes" && <Changes env={env} onClose={onClose} onOpen={(seq) => onDrawer({ kind: "change", seq })} />}
        {shown?.kind === "change" && <ChangeView env={env} seq={shown.seq} onBack={() => onDrawer({ kind: "changes" })} onClose={onClose} onChanged={onChanged} />}
        {shown?.kind === "versions" && <Versions env={env} secretKey={shown.key} type={shown.type} onClose={onClose} onChanges={() => onDrawer({ kind: "changes" })} />}
      </aside>
    </div>
  );
}

function Head({ title, sub, onBack, onClose }: { title: React.ReactNode; sub?: React.ReactNode; onBack?: () => void; onClose: () => void }) {
  return (
    <div className="flex items-center gap-2 border-b border-line px-3 py-2.5">
      {onBack ? (
        <button onClick={onBack} className="grid size-6 place-items-center rounded-md text-muted hover:bg-fg/[0.06] hover:text-fg" aria-label="Back">
          <ArrowLeft className="size-3.5" />
        </button>
      ) : (
        <HistoryIcon className="size-3.5 text-muted" />
      )}
      <div className="min-w-0 flex-1 leading-tight">
        <div className="truncate text-[13.5px] font-semibold tracking-[-0.015em]">{title}</div>
        {sub && <div className="truncate text-[11px] text-muted">{sub}</div>}
      </div>
      <button onClick={onClose} aria-label="Close" className="grid size-6 place-items-center rounded-md text-muted hover:bg-fg/[0.06] hover:text-fg">
        <X className="size-3.5" />
      </button>
    </div>
  );
}

const PAGE = 50;

function Changes({ env, onClose, onOpen }: { env: Environment; onClose: () => void; onOpen: (seq: number) => void }) {
  const { api } = useSecrets();
  const first = useLoad(() => api.changes(env.slug, undefined, PAGE), [api, env.slug]);
  const [more, setMore] = useState<Change[]>([]);
  const [end, setEnd] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setMore([]);
    setEnd(false);
  }, [first.data]);
  const rows = [...(first.data ?? []), ...more];
  const older = () => {
    const last = rows[rows.length - 1];
    if (!last) return;
    api.changes(env.slug, last.seq, PAGE).then(
      (r) => {
        setMore((m) => [...m, ...r]);
        if (r.length < PAGE) setEnd(true);
      },
      (e) => setError(errorText(e)),
    );
  };
  return (
    <>
      <Head title={`${env.name} history`} sub="Every write is a change; roll back to any of them." onClose={onClose} />
      <div className="min-h-0 flex-1 overflow-y-auto p-1.5">
        <ErrorNote error={first.error ?? error} className="m-1.5" />
        {first.data && rows.length === 0 && <p className="px-2 py-6 text-center text-[12px] text-subtle">No changes yet.</p>}
        {rows.map((c) => (
          <button key={c.seq} onClick={() => onOpen(c.seq)} className="flex w-full items-start gap-2.5 rounded-lg px-2 py-1.5 text-left transition-colors hover:bg-fg/[0.04]">
            <span className="w-9 shrink-0 pt-px text-right font-mono text-[11px] text-subtle tabular-nums">#{c.seq}</span>
            <span className="min-w-0 flex-1 leading-tight">
              <span className="block truncate text-[12.5px]">{c.message || `${c.keys} keys`}</span>
              <span className="mt-0.5 flex items-center gap-1.5 text-[11px] text-subtle">
                <span className="rounded border border-line-2 px-1 text-[10px]">{KIND_LABEL[c.kind] ?? c.kind}</span>
                {c.keys} {c.keys === 1 ? "key" : "keys"}
                {c.path !== "/" && <span className="font-mono">{c.path}</span>} · <ActorMark who={c.by} /> · {ago(c.at)}
              </span>
            </span>
          </button>
        ))}
        {first.data && first.data.length >= PAGE && !end && (
          <button onClick={older} className={`${ghost} mx-auto my-2 flex`}>
            Older changes
          </button>
        )}
      </div>
    </>
  );
}

function ChangeView({ env, seq, onBack, onClose, onChanged }: { env: Environment; seq: number; onBack: () => void; onClose: () => void; onChanged: () => void }) {
  const { api } = useSecrets();
  const detail = useLoad(() => api.change(env.slug, seq), [api, env.slug, seq]);
  const [confirm, setConfirm] = useState(false);
  const d = detail.data;
  return (
    <>
      <Head title={`Change #${seq}`} sub={d ? `${d.kind} · ${stamp(d.at)}` : undefined} onBack={onBack} onClose={onClose} />
      <div className="min-h-0 flex-1 overflow-y-auto p-3">
        <ErrorNote error={detail.error} />
        {d && (
          <>
            <p className="text-[12.5px]">{d.message || "No message"}</p>
            <p className="mt-0.5 flex items-center gap-1.5 text-[11px] text-subtle">
              <ActorMark who={d.by} /> · {ago(d.at)} {d.path !== "/" && <span className="font-mono">· {d.path}</span>}
            </p>
            <ul className="mt-3 divide-y divide-line rounded-lg border border-line-2 bg-panel px-2.5 inset-shadow-highlight">
              {d.items.map((i) => (
                <li key={`${i.path}:${i.key}:${i.type}`} className="py-1.5">
                  <ItemWhat item={i} />
                </li>
              ))}
            </ul>
            <p className="mt-2 text-[11px] text-subtle">Fingerprints show which values changed without revealing them.</p>
            <button className={`${button} mt-4`} onClick={() => setConfirm(true)}>
              <RotateCcw className="size-3.5" /> Roll back to #{seq}
            </button>
          </>
        )}
      </div>
      <Rollback env={env} seq={seq} open={confirm} onClose={() => setConfirm(false)} onDone={() => (setConfirm(false), onChanged(), onBack())} />
    </>
  );
}

/** Confirm a rollback: one new change that brings the environment back to how it was after #seq. */
function Rollback({ env, seq, open, onClose, onDone }: { env: Environment; seq: number; open: boolean; onClose: () => void; onDone: () => void }) {
  const { api } = useSecrets();
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [protectedErr, setProtectedErr] = useState(false);
  const run = async (bypass: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.rollback(env.slug, { to: seq, message: message || undefined, bypass: bypass || undefined });
      onDone();
    } catch (e) {
      if (needsApproval(e)) setProtectedErr(true);
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onClose={onClose}
      busy={busy}
      title={`Roll ${env.name} back to #${seq}?`}
      lead="This writes one new change that sets keys that changed since, restores keys that were deleted and deletes keys that didn't exist yet. History stays: the rollback can itself be rolled back."
      footer={
        <>
          <button className={ghost} onClick={onClose}>
            Cancel
          </button>
          {protectedErr ? (
            <button className={primary} disabled={busy} onClick={() => run(true)} title="Admins only; recorded in the audit log">
              Roll back without approval
            </button>
          ) : (
            <button className={primary} disabled={busy} onClick={() => run(false)}>
              {busy ? "Rolling back…" : "Roll back"}
            </button>
          )}
        </>
      }
    >
      <input value={message} onChange={(e) => setMessage(e.target.value)} placeholder={`rolled back to #${seq}`} className={field} aria-label="Message" />
      <ErrorNote error={error} className="mt-2" />
      {protectedErr && <p className="mt-2 text-[11.5px] text-muted">{env.name} is protected. An admin can roll back without approval; it's recorded in the audit log.</p>}
    </Dialog>
  );
}

function Versions({ env, secretKey, type, onClose, onChanges }: { env: Environment; secretKey: string; type: SecretType; onClose: () => void; onChanges: () => void }) {
  const { api, search, resourceName } = useSecrets();
  const path = search.path || "/";
  const owner = type === "personal" ? "personal" : undefined;
  const versions = useLoad(() => api.versions(env.slug, secretKey, path, owner), [api, env.slug, secretKey, path, type]);
  // A version next to the one before it: both revealed (each read audited), masked again like Reveal.
  const [cmp, setCmp] = useState<{ version: number; before: Shown; after: Shown; labels: [string, string] } | null>(null);
  const [loading, setLoading] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  useMaskAgain(cmp, () => setCmp(null));
  const side = async (v: Version): Promise<Shown> => {
    if (v.op === "delete") return { value: null, link: null, empty: "deleted" };
    if (v.link && v.link.mode !== "managed") return { value: null, link: v.link, empty: "" };
    return { value: (await api.versionValue(env.slug, secretKey, v.version, path, owner)).value, link: v.link, empty: "" };
  };
  const compare = async (v: Version, prev: Version) => {
    if (cmp?.version === v.version) return setCmp(null);
    setLoading(v.version);
    setError(null);
    try {
      const [before, after] = await Promise.all([side(prev), side(v)]);
      setCmp({ version: v.version, before, after, labels: [`v${prev.version}`, `v${v.version}`] });
    } catch (e) {
      setError(errorText(e));
    } finally {
      setLoading(null);
    }
  };
  const list = versions.data ?? [];
  return (
    <>
      <Head title={<span className="font-mono">{secretKey}</span>} sub={`Versions in ${env.name}${type === "personal" ? " (your override)" : ""}`} onClose={onClose} onBack={onChanges} />
      <div className="min-h-0 flex-1 overflow-y-auto p-1.5">
        <ErrorNote error={error ?? versions.error} onDismiss={error ? () => setError(null) : undefined} className="m-1.5" />
        {list.map((v, i) => (
          <div key={v.version} className="rounded-lg px-2 py-2 hover:bg-fg/[0.03]">
            <div className="flex items-center gap-2 text-[11.5px]">
              <span className="font-mono text-fg tabular-nums">v{v.version}</span>
              {v.what.map((w) => (
                <span key={w} className={`rounded border px-1 text-[10.5px] ${w === "deleted" ? "border-danger/40 text-danger" : w === "added" ? "border-ok/40 text-ok" : "border-line-2 text-muted"}`}>
                  {w}
                </span>
              ))}
              <span className="ml-auto font-mono text-[10.5px] text-subtle">#{v.seq}</span>
            </div>
            <div className="mt-0.5 flex items-center gap-1.5 text-[11px] text-subtle">
              <ActorMark who={v.by} /> · {ago(v.at)}
              {v.fp && <span className="font-mono text-faint">· {v.fp.slice(0, 8)}</span>}
            </div>
            {v.link && (
              <div className="mt-1">
                <LinkChip link={v.link} name={resourceName(v.link)} />
              </div>
            )}
            {v.comment && <p className="mt-1 text-[11.5px] text-muted">{v.comment}</p>}
            {v.op === "set" && !(v.link && v.link.mode !== "managed") && <Reveal className="mt-1" load={() => api.versionValue(env.slug, secretKey, v.version, path, owner)} />}
            {list[i + 1] && (
              <button className={`${ghost} mt-0.5 -ml-1 h-6 text-[11.5px]`} onClick={() => compare(v, list[i + 1])} aria-expanded={cmp?.version === v.version} title="Reveals both values (audited)">
                <Columns2 className="size-3.5" /> {cmp?.version === v.version ? "Hide the comparison" : loading === v.version ? "Revealing…" : `Compare with the previous version`}
              </button>
            )}
            {cmp?.version === v.version && <ValueCompare before={cmp.before} after={cmp.after} labels={cmp.labels} />}
          </div>
        ))}
      </div>
    </>
  );
}
