"use client";

import { Pencil, Plus, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { agoMs, formatDate } from "@/lib/storage-api";
import {
  iamApi,
  iamDate,
  iamNameError,
  iamTagsError,
  isAwsManaged,
  policyDocumentError,
  prettyPolicy,
  starterPolicy,
  type IamTag,
  type ManagedPolicy,
  type PolicyTarget,
} from "@/lib/storage-iam";
import { errorMessage } from "@/lib/storage-session";
import { iconBtn } from "../../tasks/ui";
import { btn, ErrorNote, input, primary } from "../../ui";
import { Field, InlineConfirm, useAction } from "../shared";

/** Load something once per `key` (and on `reload`); the last answer wins. */
export function useLoad<T>(key: string, fn: () => Promise<T>) {
  const [data, setData] = useState<T | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const fnRef = useRef(fn);
  fnRef.current = fn;
  useEffect(() => {
    let alive = true;
    fnRef.current().then(
      (d) => {
        if (!alive) return;
        setData(d);
        setLoaded(true);
        setError(null);
      },
      (e) => alive && setError(errorMessage(e)),
    );
    return () => {
      alive = false;
    };
  }, [key, tick]);
  const reload = useCallback(async () => {
    try {
      const d = await fnRef.current();
      setData(d);
      setLoaded(true);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  return { data, loaded, error, reload, refetch: () => setTick((t) => t + 1) };
}

/** An IAM date, relative, with the full time on hover. */
export function When({ iso, prefix = "" }: { iso: string | null | undefined; prefix?: string }) {
  const t = iamDate(iso);
  if (t == null) return <span>never</span>;
  return (
    <span title={formatDate(t)}>
      {prefix}
      {agoMs(t)}
    </span>
  );
}

/** A titled part of a detail panel. */
export function Section({ title, aside, children }: { title: ReactNode; aside?: ReactNode; children: ReactNode }) {
  return (
    <section className="border-t border-line px-5 py-4 first:border-0">
      <div className="mb-3 flex min-h-7 items-center justify-between gap-3">
        <h3 className="text-[13px] font-medium">{title}</h3>
        {aside}
      </div>
      {children}
    </section>
  );
}

export const Muted = ({ children }: { children: ReactNode }) => <p className="text-[12px] text-subtle">{children}</p>;

export const Loading = ({ label = "Loading…" }: { label?: string }) => (
  <p className="text-[12px] text-subtle">
    <span className="shimmer">{label}</span>
  </p>
);

/** A small outlined label (status, kind). */
export function Badge({ children, title, tone = "muted" }: { children: ReactNode; title?: string; tone?: "muted" | "ok" | "danger" | "accent" }) {
  const color = tone === "ok" ? "text-ok" : tone === "danger" ? "text-danger" : tone === "accent" ? "text-accent" : "text-muted";
  return (
    <span title={title} className={`inline-flex shrink-0 items-center gap-1 rounded-md border border-line px-1.5 py-px font-mono text-[10px] ${color}`}>
      {children}
    </span>
  );
}

/** One row of a list inside a section, with actions on the right. */
export function Row({ children, actions }: { children: ReactNode; actions?: ReactNode }) {
  return (
    <li className="rise flex min-h-9 items-center gap-3 border-b border-line py-1.5 last:border-0">
      <div className="min-w-0 flex-1">{children}</div>
      {actions && <span className="flex shrink-0 items-center gap-0.5">{actions}</span>}
    </li>
  );
}

/** A delete icon that asks first. */
export function DeleteButton({
  label,
  confirm = "Delete",
  busy,
  onConfirm,
}: {
  label: string;
  confirm?: string;
  busy: boolean;
  onConfirm: () => Promise<unknown>;
}) {
  const [asking, setAsking] = useState(false);
  if (asking)
    return (
      <InlineConfirm
        label={confirm}
        busy={busy}
        onConfirm={async () => {
          await onConfirm();
          setAsking(false);
        }}
        onCancel={() => setAsking(false)}
      />
    );
  return (
    <button type="button" onClick={() => setAsking(true)} className={`${iconBtn} hover:text-danger`} title={label} aria-label={label}>
      <Trash2 className="size-3.5" />
    </button>
  );
}

/** A selectable list of entities: name in Geist Mono, a line of detail under it. */
export function EntityList<T>({
  items,
  keyOf,
  selected,
  onSelect,
  title,
  meta,
  empty,
}: {
  items: T[] | null;
  keyOf: (t: T) => string;
  selected: string | null;
  onSelect: (key: string) => void;
  title: (t: T) => ReactNode;
  meta?: (t: T) => ReactNode;
  empty: ReactNode;
}) {
  if (items === null)
    return (
      <div className="px-5 py-6">
        <Loading />
      </div>
    );
  if (items.length === 0) return <div className="px-5 py-6 text-[13px] text-muted">{empty}</div>;
  return (
    <ul>
      {items.map((it) => {
        const k = keyOf(it);
        return (
          <li key={k} className="border-b border-line last:border-0">
            <button
              type="button"
              onClick={() => onSelect(k)}
              aria-current={k === selected ? "true" : undefined}
              className={`block w-full px-5 py-3 text-left transition ${k === selected ? "bg-panel-2" : "hover:bg-panel-2/60"}`}
            >
              <span className="block truncate font-mono text-[13px]">{title(it)}</span>
              {meta && <span className="mt-0.5 block truncate text-[11px] text-subtle">{meta(it)}</span>}
            </button>
          </li>
        );
      })}
    </ul>
  );
}

/** A one-field create form (a name), shown above a list. */
export function NameForm({
  kind,
  placeholder,
  busy,
  onCreate,
  onCancel,
  extra,
}: {
  kind: "user" | "group";
  placeholder: string;
  busy: boolean;
  onCreate: (name: string) => Promise<boolean>;
  onCancel: () => void;
  extra?: ReactNode;
}) {
  const [name, setName] = useState("");
  const err = name ? iamNameError(kind, name) : null;
  return (
    <form
      className="rise space-y-2 border-b border-line px-5 py-4"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!name || err) return;
        if (await onCreate(name)) setName("");
      }}
    >
      <div className="flex gap-2">
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={placeholder}
          spellCheck={false}
          aria-label={`New ${kind} name`}
          className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
        />
        <button type="submit" disabled={busy || !name || !!err} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : "Create"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
      {err && <p className="text-[12px] text-danger">{err}</p>}
      {extra}
    </form>
  );
}

// --- Policy documents --------------------------------------------------------------------------

/**
 * A JSON policy editor: a monospace textarea, checked as JSON while typing; the server's message
 * (IAM's MalformedPolicyDocument, say) shows under it.
 */
export function PolicyEditor({
  value,
  onChange,
  serverError,
  label,
  readOnly,
  minRows = 8,
}: {
  value: string;
  onChange?: (v: string) => void;
  serverError?: string | null;
  label: string;
  readOnly?: boolean;
  minRows?: number;
}) {
  const local = value.trim() && !readOnly ? policyDocumentError(value) : null;
  const shown = local ?? serverError ?? null;
  return (
    <div className="space-y-1.5">
      <textarea
        value={value}
        onChange={(e) => onChange?.(e.target.value)}
        readOnly={readOnly}
        rows={Math.min(24, Math.max(minRows, value.split("\n").length + 1))}
        spellCheck={false}
        aria-label={label}
        aria-invalid={!!shown}
        placeholder={'{\n  "Version": "2012-10-17",\n  "Statement": [ … ]\n}'}
        className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed ${
          shown ? "border-danger/50 focus:border-danger/70" : ""
        } ${readOnly ? "text-muted" : ""}`}
      />
      {shown && <p className="rise font-mono text-[11px] whitespace-pre-wrap text-danger">{shown}</p>}
    </div>
  );
}

/**
 * Edit a document and save it with `onSave`; the save's error (MalformedPolicyDocument and the
 * like) shows under the editor rather than in a banner.
 */
export function PolicyDocForm({
  initial,
  label,
  saveLabel,
  onSave,
  onCancel,
  name,
  children,
}: {
  initial: string;
  label: string;
  saveLabel: string;
  onSave: (doc: string, name: string) => Promise<unknown>;
  onCancel?: () => void;
  /** Ask for a name too (inline policies), with this placeholder. */
  name?: { placeholder: string; kind: "policy" };
  children?: ReactNode;
}) {
  const [doc, setDoc] = useState(initial);
  const [policyName, setPolicyName] = useState("");
  const { busy, error, setError, run } = useAction(async () => {});
  const nameErr = name && policyName ? iamNameError(name.kind, policyName) : null;
  const ok = !policyDocumentError(doc) && (!name || (policyName && !nameErr));
  return (
    <form
      className="rise space-y-3"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!ok) return;
        await run("save", () => onSave(doc, policyName));
      }}
    >
      {name && (
        <Field label="Policy name">
          <input
            value={policyName}
            onChange={(e) => setPolicyName(e.target.value)}
            placeholder={name.placeholder}
            spellCheck={false}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
      )}
      {nameErr && <p className="text-[12px] text-danger">{nameErr}</p>}
      <PolicyEditor
        value={doc}
        onChange={(v) => {
          setDoc(v);
          setError(null);
        }}
        serverError={error}
        label={label}
      />
      {children}
      <div className="flex gap-2">
        <button type="submit" disabled={busy !== null || !ok} className={primary}>
          {busy ? <span className="shimmer">Saving…</span> : saveLabel}
        </button>
        {onCancel && (
          <button type="button" onClick={onCancel} className={btn}>
            Cancel
          </button>
        )}
      </div>
    </form>
  );
}

const targetKey = (t: PolicyTarget) => `${t.kind}:${t.name}`;

/** Managed policies attached to a user, group or role: list, attach, detach. */
export function AttachedPolicies({ target, onChanged }: { target: PolicyTarget; onChanged?: () => void }) {
  const attached = useLoad(`attached:${targetKey(target)}`, () => iamApi.listAttached(target));
  const [picking, setPicking] = useState(false);
  const [all, setAll] = useState<ManagedPolicy[] | null>(null);
  const [pick, setPick] = useState("");
  const { busy, error, run } = useAction(async () => {
    await attached.reload();
    onChanged?.();
  });
  useEffect(() => {
    if (!picking || all) return;
    iamApi.listPolicies("All").then(setAll, () => setAll([]));
  }, [picking, all]);
  const have = new Set((attached.data ?? []).map((p) => p.PolicyArn));
  const choices = (all ?? []).filter((p) => !have.has(p.Arn));
  return (
    <Section
      title="Attached policies"
      aside={
        !picking && (
          <button type="button" onClick={() => setPicking(true)} className={btn}>
            <Plus className="size-3.5" /> Attach
          </button>
        )
      }
    >
      <ErrorNote error={error ?? attached.error} />
      {picking && (
        <form
          className="rise mb-3 flex flex-wrap gap-2"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!pick) return;
            const ok = await run("attach", () => iamApi.attach(target, pick));
            if (ok !== undefined) {
              setPick("");
              setPicking(false);
            }
          }}
        >
          <select
            value={pick}
            onChange={(e) => setPick(e.target.value)}
            className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
            aria-label="Policy to attach"
          >
            <option value="">{all === null ? "Loading policies…" : choices.length ? "Choose a policy" : "No other policies"}</option>
            {choices.map((p) => (
              <option key={p.Arn} value={p.Arn}>
                {p.PolicyName}
                {isAwsManaged(p.Arn) ? " (AWS managed)" : ""}
              </option>
            ))}
          </select>
          <button type="submit" disabled={!pick || busy !== null} className={primary}>
            {busy === "attach" ? <span className="shimmer">Attaching…</span> : "Attach"}
          </button>
          <button type="button" onClick={() => setPicking(false)} className={btn}>
            Cancel
          </button>
        </form>
      )}
      {attached.data === null ? (
        <Loading />
      ) : attached.data.length === 0 ? (
        <Muted>No managed policies attached.</Muted>
      ) : (
        <ul>
          {attached.data.map((p) => (
            <Row
              key={p.PolicyArn}
              actions={
                <DeleteButton
                  label={`Detach ${p.PolicyName}`}
                  confirm="Detach"
                  busy={busy === `detach-${p.PolicyArn}`}
                  onConfirm={() => run(`detach-${p.PolicyArn}`, () => iamApi.detach(target, p.PolicyArn))}
                />
              }
            >
              <p className="flex items-center gap-2 truncate text-[13px]">
                <span className="truncate font-mono">{p.PolicyName}</span>
                {isAwsManaged(p.PolicyArn) && <Badge>AWS managed</Badge>}
              </p>
              <p className="truncate font-mono text-[11px] text-subtle">{p.PolicyArn}</p>
            </Row>
          ))}
        </ul>
      )}
    </Section>
  );
}

/** Inline policies of a user, group or role: list, view/edit, add, delete. */
export function InlinePolicies({ target }: { target: PolicyTarget }) {
  const names = useLoad(`inline:${targetKey(target)}`, () => iamApi.listInline(target));
  const [open, setOpen] = useState<string | null>(null);
  const [doc, setDoc] = useState<{ name: string; text: string } | null>(null);
  const [adding, setAdding] = useState(false);
  const [loadErr, setLoadErr] = useState<string | null>(null);
  const { busy, error, run } = useAction(names.reload);

  const show = async (name: string) => {
    if (open === name) {
      setOpen(null);
      return;
    }
    setOpen(name);
    setDoc(null);
    setLoadErr(null);
    try {
      setDoc({ name, text: prettyPolicy(await iamApi.getInline(target, name)) });
    } catch (e) {
      setLoadErr(errorMessage(e));
    }
  };

  return (
    <Section
      title="Inline policies"
      aside={
        !adding && (
          <button type="button" onClick={() => setAdding(true)} className={btn}>
            <Plus className="size-3.5" /> Add
          </button>
        )
      }
    >
      <ErrorNote error={error ?? names.error} />
      {adding && (
        <div className="mb-4">
          <PolicyDocForm
            initial={starterPolicy()}
            label="Inline policy JSON"
            saveLabel="Add policy"
            name={{ placeholder: "read-reports", kind: "policy" }}
            onCancel={() => setAdding(false)}
            onSave={async (d, n) => {
              await iamApi.putInline(target, n, d);
              setAdding(false);
              await names.reload();
            }}
          />
        </div>
      )}
      {names.data === null ? (
        <Loading />
      ) : names.data.length === 0 ? (
        !adding && <Muted>No inline policies.</Muted>
      ) : (
        <ul>
          {names.data.map((n) => (
            <li key={n} className="border-b border-line py-1.5 last:border-0">
              <div className="flex min-h-9 items-center gap-3">
                <button type="button" onClick={() => show(n)} className="min-w-0 flex-1 truncate text-left font-mono text-[13px] hover:text-fg">
                  {n}
                </button>
                <button
                  type="button"
                  onClick={() => show(n)}
                  className={iconBtn}
                  title={open === n ? "Close" : "View and edit"}
                  aria-label={open === n ? `Close ${n}` : `Edit ${n}`}
                >
                  {open === n ? <X className="size-3.5" /> : <Pencil className="size-3.5" />}
                </button>
                <DeleteButton
                  label={`Delete ${n}`}
                  busy={busy === `del-${n}`}
                  onConfirm={() =>
                    run(`del-${n}`, async () => {
                      await iamApi.deleteInline(target, n);
                      if (open === n) setOpen(null);
                    })
                  }
                />
              </div>
              {open === n && (
                <div className="pt-2 pb-2">
                  {loadErr ? (
                    <ErrorNote error={loadErr} />
                  ) : doc?.name !== n ? (
                    <Loading />
                  ) : (
                    <PolicyDocForm
                      key={doc.text}
                      initial={doc.text}
                      label={`${n} JSON`}
                      saveLabel="Save policy"
                      onSave={async (d) => {
                        await iamApi.putInline(target, n, d);
                        setDoc({ name: n, text: prettyPolicy(d) });
                      }}
                    />
                  )}
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}

/** Tags of a user or role: list, add, remove. */
export function TagsSection({ target }: { target: Exclude<PolicyTarget, { kind: "group" }> }) {
  const tags = useLoad(`tags:${targetKey(target)}`, () => iamApi.listTags(target));
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const { busy, error, run } = useAction(tags.reload);
  const next: IamTag[] = [...(tags.data ?? []).filter((t) => t.Key !== key), { Key: key, Value: value }];
  const err = key ? iamTagsError(next) : null;
  return (
    <Section title="Tags">
      <ErrorNote error={error ?? tags.error} />
      {tags.data === null ? (
        <Loading />
      ) : (
        <>
          {tags.data.length > 0 && (
            <ul className="mb-3 flex flex-wrap gap-1.5">
              {tags.data.map((t) => (
                <li key={t.Key} className="inline-flex items-center gap-1 rounded-md border border-line py-px pr-0.5 pl-1.5 font-mono text-[11px]">
                  <span className="text-muted">{t.Key}</span>
                  <span className="text-subtle">=</span>
                  <span>{t.Value}</span>
                  <button
                    type="button"
                    disabled={busy !== null}
                    onClick={() => run(`untag-${t.Key}`, () => iamApi.untag(target, [t.Key]))}
                    className="p-0.5 text-subtle transition hover:text-danger"
                    aria-label={`Remove tag ${t.Key}`}
                    title="Remove"
                  >
                    <X className="size-3" />
                  </button>
                </li>
              ))}
            </ul>
          )}
          <form
            className="flex flex-wrap gap-2"
            onSubmit={async (e) => {
              e.preventDefault();
              if (!key || err) return;
              const ok = await run("tag", () => iamApi.tag(target, [{ Key: key, Value: value }]));
              if (ok !== undefined) {
                setKey("");
                setValue("");
              }
            }}
          >
            <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="key" spellCheck={false} aria-label="Tag key" className={`${input} w-32 font-mono text-[12px]`} />
            <input
              value={value}
              onChange={(e) => setValue(e.target.value)}
              placeholder="value"
              spellCheck={false}
              aria-label="Tag value"
              className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
            />
            <button type="submit" disabled={!key || !!err || busy !== null} className={btn}>
              {busy === "tag" ? <span className="shimmer">Saving…</span> : tags.data.some((t) => t.Key === key) ? "Update tag" : "Add tag"}
            </button>
          </form>
          {err && <p className="mt-2 text-[12px] text-danger">{err}</p>}
        </>
      )}
    </Section>
  );
}

/** The detail panel's header: name, ARN, delete. */
export function DetailHeader({
  name,
  arn,
  meta,
  onDelete,
  deleteHint,
  busy,
}: {
  name: string;
  arn: string;
  meta?: ReactNode;
  onDelete?: () => Promise<unknown>;
  deleteHint?: string;
  busy: boolean;
}) {
  return (
    <div className="flex items-start justify-between gap-3 border-b border-line px-5 py-4">
      <div className="min-w-0 space-y-0.5">
        <h2 className="truncate font-mono text-[15px] font-medium">{name}</h2>
        <p className="truncate font-mono text-[11px] text-subtle" title={arn}>
          {arn}
        </p>
        {meta && <div className="text-[12px] text-muted">{meta}</div>}
      </div>
      {onDelete && (
        <span title={deleteHint}>
          <DeleteButton label={`Delete ${name}`} busy={busy} onConfirm={onDelete} />
        </span>
      )}
    </div>
  );
}

/** The two-column layout of an IAM tab: a list and the selected entity. */
export function MasterDetail({ list, detail }: { list: ReactNode; detail: ReactNode }) {
  return (
    <div className="grid items-start gap-4 lg:grid-cols-5">
      <div className="lg:col-span-2">{list}</div>
      <div className="min-w-0 lg:col-span-3">{detail}</div>
    </div>
  );
}

export function NothingSelected({ children }: { children: ReactNode }) {
  return <div className="rounded-xl border border-dashed border-line px-5 py-10 text-center text-[13px] text-subtle">{children}</div>;
}
