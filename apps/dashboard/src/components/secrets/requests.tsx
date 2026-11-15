import { Check, Eye, EyeOff, GitPullRequestArrow, Lock, X } from "lucide-react";
import { useState } from "react";
import type { Batch, ChangeRequest, Environment, Item, RequestState, RequestValue } from "@/lib/secrets-api";
import { Segmented } from "../agents/ui/toggle-group";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Teach, useLoad, useMaskAgain } from "./ui";
import { ValueCompare, type Shown } from "./value-compare";

/** A write a protected environment refused, kept so it can become a change request. */
export type Pending = { batch: Batch; why: string; label: string };

/** "production is protected": open a change request with the same operations, and a message for the reviewers. */
export function ApprovalOffer({ env, pending, onOpened, onCancel }: { env: Environment; pending: Pending; onOpened: (r: ChangeRequest) => void; onCancel: () => void }) {
  const { api } = useSecrets();
  const [message, setMessage] = useState(pending.batch.message ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const keys = [...new Set(pending.batch.ops.map((o) => o.key))];
  const openIt = async () => {
    setBusy(true);
    setError(null);
    try {
      onOpened(await api.openRequest(env.slug, { ...pending.batch, message: message || undefined }));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="fade-in rounded-xl border border-accent/40 bg-panel p-3 shadow-card inset-shadow-highlight" role="alert">
      <div className="flex items-start gap-2">
        <Lock className="mt-0.5 size-3.5 shrink-0 text-accent" />
        <div className="min-w-0 flex-1 text-[12.5px]">
          <b className="font-medium">{env.name} is protected.</b> <span className="text-muted">{pending.why}.</span>
          <div className="mt-0.5 text-[11.5px] text-muted">
            Open a change request for {keys.length ? <span className="font-mono text-soft">{keys.join(", ")}</span> : "this change"}: it applies once it has {Math.max(1, env.approvals)}{" "}
            {env.approvals > 1 ? "approvals" : "approval"} from someone other than you.
          </div>
        </div>
      </div>
      <div className="mt-2.5 flex flex-wrap items-center gap-2 pl-5.5">
        <input value={message} onChange={(e) => setMessage(e.target.value)} placeholder="Why (for the reviewers)" className={`${field} max-w-sm flex-1`} />
        <button className={primary} onClick={openIt} disabled={busy}>
          <GitPullRequestArrow className="size-3.5" /> {busy ? "Opening…" : "Open a change request"}
        </button>
        <button className={ghost} onClick={onCancel}>
          Discard
        </button>
      </div>
      <ErrorNote error={error} className="mt-2" />
    </div>
  );
}

const STATE_TONE: Record<RequestState, string> = {
  open: "text-accent",
  approved: "text-ok",
  applied: "text-subtle",
  rejected: "text-danger",
  closed: "text-subtle",
  stale: "text-danger",
};

/** An environment's change requests: review, apply (or bypass, as an admin), close. */
export function RequestsPanel({ env, onChanged }: { env: Environment; onChanged: () => void }) {
  const { api, project } = useSecrets();
  const [state, setState] = useState<"open" | "all">("open");
  const list = useLoad(() => api.requests(env.slug, state === "open" ? "open" : undefined), [api, env.slug, state]);
  const rows = list.data ?? [];
  return (
    <div className="space-y-3 px-3 pt-3 pb-8">
      <div className="flex items-center gap-2">
        <Segmented
          label="Requests"
          value={state}
          onChange={setState}
          options={[
            { value: "open", label: <span className="px-1.5 text-[11.5px]">Open</span> },
            { value: "all", label: <span className="px-1.5 text-[11.5px]">All</span> },
          ]}
        />
      </div>
      <ErrorNote error={list.error} />
      {list.data && rows.length === 0 && (
        <Teach title={state === "open" ? "No open change requests" : "No change requests yet"} cli={[`roll secrets envs ls -p ${project.name}`]}>
          {env.protected
            ? `Writes to ${env.name} open a request here instead of applying. Someone other than the author approves it, then it applies as one change.`
            : `${env.name} isn't protected, so writes apply at once. Protect it under Environments to require approvals.`}
        </Teach>
      )}
      {rows.map((r) => (
        <RequestCard
          key={r.id}
          env={env}
          request={r}
          onChanged={(next) => {
            list.set(rows.map((x) => (x.id === next.id ? next : x)));
            onChanged();
          }}
        />
      ))}
    </div>
  );
}

export function ItemWhat({ item }: { item: Item }) {
  return (
    <span className="flex min-w-0 items-center gap-2 text-[12px]">
      <span className="truncate font-mono">{item.key}</span>
      {item.type === "personal" && <span className="text-[10.5px] text-accent">personal</span>}
      {item.path !== "/" && <span className="font-mono text-[10.5px] text-subtle">{item.path}</span>}
      <span className="flex gap-1">
        {item.what.map((w) => (
          <span key={w} className={`rounded border px-1 text-[10.5px] ${w === "deleted" ? "border-danger/40 text-danger" : w === "added" ? "border-ok/40 text-ok" : "border-line-2 text-muted"}`}>
            {w}
          </span>
        ))}
      </span>
      <span className="ml-auto shrink-0 font-mono text-[10.5px] text-faint" title="Fingerprints: equal values have equal fingerprints; no value is shown">
        {item.from_fp?.slice(0, 6) ?? "—"} → {item.to_fp?.slice(0, 6) ?? "—"}
      </span>
    </span>
  );
}

/** A request item's values now and after it applies, for `ValueCompare`. */
function sides(i: Item, v: RequestValue): [Shown, Shown] {
  const now: Shown = { value: v.now, link: v.now_link, empty: i.what.includes("added") ? "not set" : "no stored value" };
  const after: Shown = v.delete ? { value: null, link: null, empty: "deleted" } : v.proposed !== null || v.proposed_link ? { value: v.proposed, link: v.proposed_link, empty: "" } : { ...now, empty: "unchanged" };
  return [now, after];
}

function RequestCard({ env, request: r, onChanged }: { env: Environment; request: ChangeRequest; onChanged: (r: ChangeRequest) => void }) {
  const { api } = useSecrets();
  const [comment, setComment] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const act = async (run: () => Promise<ChangeRequest>) => {
    setBusy(true);
    setError(null);
    try {
      onChanged(await run());
      setComment("");
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const live = r.state === "open" || r.state === "approved";
  // What it changes, for an approver who may reveal it: masked again like Reveal.
  const [values, setValues] = useState<RequestValue[] | null>(null);
  useMaskAgain(values, () => setValues(null));
  const reveal = async () => {
    if (values) return setValues(null);
    setError(null);
    try {
      setValues(await api.requestValues(env.slug, r.id));
    } catch (e) {
      setError(errorText(e));
    }
  };
  return (
    <article className={`${card} p-3`}>
      <header className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <span className="font-mono text-[11px] text-subtle">{r.id}</span>
        <span className="min-w-0 flex-1 truncate text-[13px] font-medium">{r.message || "No message"}</span>
        <span className={`text-[11.5px] font-medium ${STATE_TONE[r.state]}`}>{r.state}</span>
        <span className="font-mono text-[11px] text-muted tabular-nums" title="Approvals">
          {r.approvals}/{r.approvals_needed}
        </span>
      </header>
      <p className="mt-0.5 flex items-center gap-1.5 text-[11px] text-subtle">
        <ActorMark who={r.by} /> · {ago(r.at)}
        {r.path !== "/" && <span className="font-mono">· {r.path}</span>}
        {r.applied_seq !== null && <span>· applied as change #{r.applied_seq}</span>}
      </p>
      <ul className="mt-2 divide-y divide-line rounded-lg border border-line-2 bg-panel-2/60 px-2.5">
        {r.items.map((i) => {
          const v = values?.find((x) => x.key === i.key && x.path === i.path && x.type === i.type);
          return (
            <li key={`${i.path}:${i.key}:${i.type}`} className="py-1.5">
              <ItemWhat item={i} />
              {v && <ValueCompare before={sides(i, v)[0]} after={sides(i, v)[1]} labels={["now", "after"]} />}
              {v?.proposed_weak && (
                <p className="mt-1 text-[11px] text-danger" role="note">
                  The proposed value is easy to guess: {v.proposed_weak}.
                </p>
              )}
            </li>
          );
        })}
      </ul>
      {live && (
        <button className={`${ghost} mt-1.5`} onClick={reveal} aria-expanded={!!values} title={values ? "Hide the values" : "Show the values it changes, now and after (audited)"}>
          {values ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />} {values ? "Hide values" : "Show values"}
        </button>
      )}
      {r.reviews.length > 0 && (
        <ul className="mt-2 space-y-1 text-[11.5px]">
          {r.reviews.map((v) => (
            <li key={`${v.by}:${v.at}`} className="flex items-center gap-1.5 text-muted">
              {v.verdict === "approve" ? <Check className="size-3.5 text-ok" /> : <X className="size-3.5 text-danger" />}
              <ActorMark who={v.by} />
              {v.comment && <span className="truncate text-soft">“{v.comment}”</span>}
              <span className="ml-auto text-subtle">{ago(v.at)}</span>
            </li>
          ))}
        </ul>
      )}
      {r.state === "stale" && <p className="mt-2 text-[11.5px] text-danger">A key it touches changed since it was opened. Close it and open a fresh one.</p>}
      {live && (
        <div className="mt-2.5 flex flex-wrap items-center gap-1.5">
          {r.state === "open" && (
            <>
              <input value={comment} onChange={(e) => setComment(e.target.value)} placeholder="Comment (optional)" className={`${field} h-7 max-w-xs flex-1`} />
              <button disabled={busy} className={button} onClick={() => act(() => api.review(env.slug, r.id, "approve", comment || undefined))}>
                <Check className="size-3.5" /> Approve
              </button>
              <button disabled={busy} className={dangerButton} onClick={() => act(() => api.review(env.slug, r.id, "reject", comment || undefined))}>
                Reject
              </button>
            </>
          )}
          <span className="ml-auto" />
          {r.state === "approved" ? (
            <button disabled={busy} className={primary} onClick={() => act(() => api.apply(env.slug, r.id))}>
              Apply
            </button>
          ) : (
            <button disabled={busy} className={ghost} onClick={() => act(() => api.apply(env.slug, r.id, true))} title="Apply without approvals (admin; recorded in the audit log)">
              Apply without approval
            </button>
          )}
          <button disabled={busy} className={ghost} onClick={() => act(() => api.close(env.slug, r.id))}>
            Close
          </button>
        </div>
      )}
      <ErrorNote error={error} className="mt-2" />
    </article>
  );
}
