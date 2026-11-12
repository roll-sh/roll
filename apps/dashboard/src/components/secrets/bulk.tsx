import { ArrowUpRight, FolderInput, Tag, Trash2, X } from "lucide-react";
import { useState } from "react";
import { FOLDER_PATH, TAG, type Applied, type Batch, type Environment, type Op, type Secret } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { button, dangerButton, errorText, ghost, iconButton, primary, sized } from "./ui";

/** A bulk write: runs as one change (or, on a protected environment, offers the same batch as a change request). */
export type BulkWrite = (run: () => Promise<{ warnings?: string[]; change?: { seq: number } | null }>, batch: Batch, done: string) => Promise<boolean>;

type Mode = null | "tag" | "move" | "promote" | "delete";

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
const owner = (s: Secret) => (s.type === "personal" ? ("personal" as const) : undefined);

/**
 * The bar over an environment's list while rows are selected: delete, add or remove tags, move to
 * another folder, or promote to another environment. Each is one change with every operation.
 */
export function BulkBar({ env, path, picked, onWrite, onDone, onClear, onNote }: { env: Environment; path: string; picked: Secret[]; onWrite: BulkWrite; onDone: () => void; onClear: () => void; onNote: (text: string) => void }) {
  const { api, standard } = useSecrets();
  const [mode, setMode] = useState<Mode>(null);
  const [tags, setTags] = useState("");
  const [to, setTo] = useState("");
  const [target, setTarget] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [replace, setReplace] = useState<string[] | null>(null);
  const n = picked.length;
  const shared = picked.filter((s) => s.type !== "personal");
  const personal = n - shared.length;
  const tagList = tags.toLowerCase().split(/[\s,]+/).filter(Boolean);
  const tagsOk = tagList.length > 0 && tagList.every((t) => TAG.test(t));
  const dest = to.trim().replace(/\/+$/, "") || "/";
  const destOk = FOLDER_PATH.test(dest) && dest !== path;
  const targets = standard.filter((e) => e.slug !== env.slug);
  const promoteTo = target ?? targets[0]?.slug ?? null;

  /** Runs one action; anything but `false` (the write was refused or needs approval) closes its form. */
  const run = async (f: () => unknown) => {
    setBusy(true);
    setError(null);
    try {
      if ((await f()) !== false) {
        setMode(null);
        setReplace(null);
        onDone();
      }
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const del = () =>
    run(() => {
      const ops: Op[] = picked.map((s) => ({ op: "delete", key: s.key, type: owner(s) }));
      const batch: Batch = { path, ops, message: `deleted ${plural(n, "key")}` };
      return onWrite(() => api.batch(env.slug, batch), batch, `Deleted ${plural(n, "key")}`);
    });

  const tag = (add: boolean) =>
    run(() => {
      const ops: Op[] = picked.flatMap((s): Op[] => {
        const next = add ? [...new Set([...s.tags, ...tagList])] : s.tags.filter((t) => !tagList.includes(t));
        return next.length === s.tags.length && next.every((t, i) => t === s.tags[i]) ? [] : [{ op: "set", key: s.key, type: owner(s), tags: next }];
      });
      if (!ops.length) return void onNote(add ? "They already have those tags" : "None of them has those tags");
      const what = `${add ? "tagged" : "untagged"} ${plural(ops.length, "key")} ${tagList.join(", ")}`;
      const batch: Batch = { path, ops, message: what };
      return onWrite(() => api.batch(env.slug, batch), batch, what[0].toUpperCase() + what.slice(1));
    });

  /** Each shared secret deleted here and set at `dest` with its own value (revealed unresolved, audited) or its link. */
  const move = (confirmed: boolean) =>
    run(async () => {
      if (!confirmed) {
        const there = new Set((await api.secrets(env.slug, dest).catch(() => [])).filter((s) => s.source.kind === "own" && s.type === "shared").map((s) => s.key));
        const clash = shared.filter((s) => there.has(s.key)).map((s) => s.key);
        if (clash.length) return void setReplace(clash);
      }
      const sets = await Promise.all(
        shared.map(async (s): Promise<Op> => {
          // A linked secret keeps its link and stores no value of its own here (a managed one is issued anew).
          const value = s.link ? undefined : (await api.value(env.slug, s.key, path, false)).value;
          return { op: "set", key: s.key, path: dest, value, comment: s.comment, tags: s.tags, link: s.link ?? undefined };
        }),
      );
      const ops: Op[] = [...shared.map((s): Op => ({ op: "delete", key: s.key, path })), ...sets];
      const what = `moved ${plural(shared.length, "key")} to ${dest}`;
      const batch: Batch = { path, ops, message: what };
      const ok = await onWrite(() => api.batch(env.slug, batch), batch, `Moved ${plural(shared.length, "key")} to ${dest}${personal ? ` (${plural(personal, "personal override")} skipped)` : ""}`);
      return ok;
    });

  const promote = () =>
    run(async () => {
      if (!promoteTo) return false;
      const keys = shared.map((s) => s.key);
      const r: Applied = await api.promote(promoteTo, env.slug, { path, keys, message: `promoted ${plural(keys.length, "key")} from ${env.slug}` });
      const skipped = personal ? ` (${plural(personal, "personal override")} skipped)` : "";
      onNote(
        r.request
          ? `${promoteTo} is protected: opened change request ${r.request.id} with ${plural(keys.length, "key")}${skipped}`
          : r.change
            ? `Promoted ${plural(keys.length, "key")} to ${promoteTo} · change #${r.change.seq}${skipped}`
            : `${promoteTo} already has these values${skipped}`,
      );
    });

  return (
    <div role="toolbar" aria-label="Selected secrets" className="fade-in sticky top-0 z-10 mb-2 rounded-lg border border-line-2 bg-panel/95 px-2 py-1.5 backdrop-blur inset-shadow-highlight">
      <div className="flex flex-wrap items-center gap-1.5">
        <span className="px-1 text-[12px] font-medium tabular-nums">{n} selected</span>
        {personal > 0 && <span className="text-[11px] text-subtle">({plural(personal, "personal override")})</span>}
        <span className="mx-1 h-4 w-px bg-line-2" />
        <button className={mode === "tag" ? button : ghost} onClick={() => setMode(mode === "tag" ? null : "tag")}>
          <Tag className="size-3.5" /> Tags
        </button>
        <button className={mode === "move" ? button : ghost} onClick={() => setMode(mode === "move" ? null : "move")} disabled={!shared.length} title={shared.length ? undefined : "Personal overrides don't move"}>
          <FolderInput className="size-3.5" /> Move
        </button>
        <button className={mode === "promote" ? button : ghost} onClick={() => setMode(mode === "promote" ? null : "promote")} disabled={!shared.length || !targets.length}>
          <ArrowUpRight className="size-3.5" /> Promote
        </button>
        <button className={mode === "delete" ? dangerButton : ghost} onClick={() => setMode(mode === "delete" ? null : "delete")}>
          <Trash2 className="size-3.5" /> Delete
        </button>
        <span className="ml-auto" />
        <button className={iconButton} onClick={onClear} aria-label="Clear the selection" title="Clear the selection (Esc)">
          <X className="size-3.5" />
        </button>
      </div>

      {mode && (
        <div className="mt-1.5 flex flex-wrap items-center gap-2 border-t border-line pt-1.5 text-[12px]">
          {mode === "delete" && (
            <>
              <span className="text-muted">Delete {plural(n, "secret")} in one change you can roll back?</span>
              <button className={dangerButton} disabled={busy} onClick={del}>
                {busy ? "Deleting…" : `Delete ${n}`}
              </button>
            </>
          )}
          {mode === "tag" && (
            <>
              <input autoFocus value={tags} onChange={(e) => setTags(e.target.value)} placeholder="billing, rotate-yearly" aria-label="Tags" className={`${sized} h-7 w-56 font-mono text-[12px]`} />
              <button className={button} disabled={!tagsOk || busy} onClick={() => tag(true)}>
                Add
              </button>
              <button className={button} disabled={!tagsOk || busy} onClick={() => tag(false)}>
                Remove
              </button>
              {tags && !tagsOk && <span className="text-[11px] text-danger">Tags are a-z, 0-9 and -, up to 32 characters.</span>}
            </>
          )}
          {mode === "move" && (
            <>
              <input
                autoFocus
                value={to}
                onChange={(e) => {
                  setTo(e.target.value.trim());
                  setReplace(null);
                }}
                placeholder="/api"
                aria-label="Move to folder"
                spellCheck={false}
                className={`${sized} h-7 w-48 font-mono text-[12px]`}
              />
              {replace ? (
                <>
                  <span className="text-[11.5px] text-danger">
                    {replace.join(", ")} {replace.length === 1 ? "exists" : "exist"} in {dest}.
                  </span>
                  <button className={dangerButton} disabled={busy} onClick={() => move(true)}>
                    Replace and move
                  </button>
                </>
              ) : (
                <button className={primary} disabled={!destOk || busy} onClick={() => move(false)}>
                  {busy ? "Moving…" : `Move ${shared.length}`}
                </button>
              )}
              <span className="text-[11px] text-subtle">
                {to && !destOk ? (dest === path ? "That's this folder." : "A path is / or /folder/…") : "Links move with them; a managed credential is issued again there."}
                {personal > 0 && ` ${plural(personal, "personal override")} stay${personal === 1 ? "s" : ""}.`}
              </span>
            </>
          )}
          {mode === "promote" && (
            <>
              <span className="text-muted">Copy {plural(shared.length, "value")} to</span>
              <Select label="Promote to" value={promoteTo} onChange={setTarget} options={targets.map((e) => ({ value: e.slug, label: e.name }))} className="h-7 w-40" />
              <button className={primary} disabled={!promoteTo || busy} onClick={promote}>
                {busy ? "Promoting…" : `Promote ${shared.length}`}
              </button>
              <span className="text-[11px] text-subtle">At {path}, as one change there; a protected environment gets a change request.</span>
            </>
          )}
          {error && (
            <span className="w-full text-[11.5px] text-danger" role="alert">
              {error}
            </span>
          )}
        </div>
      )}
    </div>
  );
}
