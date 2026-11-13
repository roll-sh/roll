import { Clock, Dices, Download, Folder, FolderPlus, History, Import as ImportIcon, Link2, Lock, MoreHorizontal, Pencil, Plus, RotateCw, Search, Send, Trash2, Upload, User } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Link } from "@/components/link";
import { canRotate, needsApproval, SECRET_KEY, TAG, type Batch, type Environment, type ExportFormat, type Import, type KeyInsight, type Op, type Secret, type SecretType } from "@/lib/secrets-api";
import { generateValue } from "@/lib/secrets-generate";
import { childPath, folderName } from "@/lib/secrets-grid";
import { misfit } from "@/lib/secrets-required";
import { Menu, MenuItem, MenuPopup, MenuTrigger } from "../agents/ui/menu";
import { Segmented } from "../agents/ui/toggle-group";
import { FolderCrumbs } from "./folders";
import { useSlashFocus } from "./grid";
import { HistoryDrawer, type Drawer } from "./history";
import { ImportDialog } from "./import-dialog";
import { ImportsPanel } from "./imports";
import { RefInput, useRefSource, useRefWarnings, type RefSource } from "./ref-input";
import { ShareDialog, type ShareSource } from "./share-dialog";
import { Activity } from "./activity";
import { BulkBar } from "./bulk";
import { LinkDialog } from "./link-dialog";
import { ApprovalOffer, RequestsPanel, type Pending } from "./requests";
import { secretsHref, useSecrets } from "./secrets-page";
import { ago, button, Chip, CommentMark, dangerButton, ErrorNote, errorText, ExpiryMark, field, ghost, iconButton, label, LinkChip, PersonalMark, primary, RefMark, Reveal, saveText, sized, SourceMark, Teach, until, useLoad, Warnings } from "./ui";

/** One environment: its folder's secrets (inline edit, reveal, add, delete, overrides), requests and history. */
export function EnvView({ env }: { env: Environment }) {
  const { api, search, go, project, reloadEnvs, standard } = useSecrets();
  const path = search.path || "/";
  const tab = search.tab === "requests" ? "requests" : search.tab === "activity" ? "activity" : "secrets";
  const list = useLoad(() => api.secrets(env.slug, path), [api, env.slug, path]);
  const folders = useLoad(() => api.folders(env.slug, path), [api, env.slug, path]);
  const open = useLoad(() => api.requests(env.slug, "open"), [api, env.slug]);
  const imports = useLoad(() => api.imports(env.slug, path), [api, env.slug, path]);
  // Last changes and reads (from the audit log): one call per folder view.
  const insights = useLoad(() => api.insights(env.slug, path), [api, env.slug, path]);
  const insightOf = useMemo(() => new Map((insights.data?.keys ?? []).map((k) => [k.key, k])), [insights.data]);
  const [unread, setUnread] = useState(false);
  /** The imports panel, open with one import picked out (an imported key's source was clicked). */
  const [importsOpen, setImportsOpen] = useState<{ highlight: Import | null } | null>(null);
  const [query, setQuery] = useState("");
  const [adding, setAdding] = useState(false);
  const [importing, setImporting] = useState(false);
  const [linking, setLinking] = useState(false);
  const [drawer, setDrawer] = useState<Drawer | null>(null);
  const [sharing, setSharing] = useState<ShareSource | null>(null);
  /** Selected rows (`key:type`), and the last one clicked, for shift-click ranges. */
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const anchor = useRef<string | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<{ text: string; warnings: string[] } | null>(null);
  const filter = useRef<HTMLInputElement>(null);
  useSlashFocus(filter);

  // `n` adds a secret, `h` opens the history (not while typing).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (e.metaKey || e.ctrlKey || e.altKey || t?.closest("input, textarea, [contenteditable], [role=dialog]")) return;
      if (e.key === "n") {
        e.preventDefault();
        setAdding(true);
      } else if (e.key === "h") setDrawer({ kind: "changes" });
      else if (e.key === "Escape") setSelected(new Set());
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const secrets = list.data ?? [];
  const keys = useMemo(() => new Set(secrets.map((s) => s.key)), [secrets]);
  const refSource = useRefSource(env.slug, keys);
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const isUnread = (s: Secret) => insightOf.get(s.key)?.last_read_at === null;
  const unreadCount = secrets.filter(isUnread).length;
  const shown = secrets.filter((s) => (!unread || isUnread(s)) && words.every((w) => s.key.toLowerCase().includes(w) || s.comment.toLowerCase().includes(w) || s.tags.some((t) => t.includes(w))));
  const childFolders = folders.data?.folders ?? [];
  const rowId = (s: Secret) => `${s.key}:${s.type}`;
  const selectable = shown.filter((s) => s.source.kind === "own");
  const picked = secrets.filter((s) => selected.has(rowId(s)));
  useEffect(() => {
    setSelected(new Set());
    anchor.current = null;
  }, [env.slug, path]);
  const select = (s: Secret, on: boolean, range: boolean) => {
    const id = rowId(s);
    const ids = selectable.map(rowId);
    const from = anchor.current ? ids.indexOf(anchor.current) : -1;
    const span = range && from >= 0 ? ids.slice(Math.min(from, ids.indexOf(id)), Math.max(from, ids.indexOf(id)) + 1) : [id];
    anchor.current = id;
    setSelected((cur) => {
      const next = new Set(cur);
      for (const x of span) on ? next.add(x) : next.delete(x);
      return next;
    });
  };
  const p = path === "/" ? undefined : path;

  const refresh = () => {
    list.reload();
    folders.reload();
    insights.reload();
    reloadEnvs();
  };

  /** Runs a write; a protected environment's 409 turns into an offer to open a change request with the same operations. */
  const write = async (run: () => Promise<{ warnings?: string[]; change?: { seq: number } | null }>, batch: Batch, done: string): Promise<boolean> => {
    setError(null);
    try {
      const r = await run();
      setNote({ text: r.change ? `${done} · change #${r.change.seq}` : `${done} · nothing changed`, warnings: r.warnings ?? [] });
      refresh();
      return true;
    } catch (e) {
      if (needsApproval(e)) setPending({ batch, why: e.message, label: done });
      else setError(errorText(e));
      return false;
    }
  };

  const exportAs = async (format: ExportFormat) => {
    setError(null);
    try {
      const ext = { dotenv: ".env", shell: ".sh", json: ".json", yaml: ".yaml", docker: ".docker.env", kubernetes: ".secret.yaml" }[format];
      saveText(`${project.name}.${env.slug}${ext}`, await api.exportText(env.slug, format, path));
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <div className="flex min-h-full">
      <div className="min-w-0 flex-1">
        <header className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 pt-4 pb-3">
          <div className="min-w-0">
            <h1 className="flex items-center gap-2 text-[17px] font-semibold tracking-[-0.03em]">
              {env.name}
              <span className="font-mono text-[11.5px] font-normal tracking-normal text-subtle">{env.slug}</span>
            </h1>
            <p className="mt-0.5 flex flex-wrap items-center gap-x-2 text-[11.5px] text-muted">
              {env.kind === "ephemeral" ? (
                <span>
                  Preview of <b className="font-medium text-soft">{env.base}</b>: keys it doesn't set come from there
                  {env.expires_at ? ` · expires ${until(env.expires_at)}` : ""}
                </span>
              ) : env.protected ? (
                <span className="flex items-center gap-1">
                  <Lock className="size-3" /> Protected: writes open a change request ({Math.max(1, env.approvals)} {env.approvals === 1 ? "approval" : "approvals"})
                </span>
              ) : (
                <span>Writes apply at once, each one a change you can roll back.</span>
              )}
            </p>
          </div>
          <span className="ml-auto flex flex-wrap items-center gap-1.5">
            <button className={primary} onClick={() => setAdding((a) => !a)} title="Add a secret (n)">
              <Plus className="size-3.5" /> Add
            </button>
            <button className={button} onClick={() => setImporting(true)}>
              <Upload className="size-3.5" /> Paste .env
            </button>
            <button className={button} onClick={() => setLinking(true)}>
              <Link2 className="size-3.5" /> Link a resource
            </button>
            <Menu>
              <MenuTrigger className={button} aria-label="Export">
                <Download className="size-3.5" /> Export
              </MenuTrigger>
              <MenuPopup align="end" className="w-48">
                {(["dotenv", "json", "shell", "yaml", "docker", "kubernetes"] as const).map((f) => (
                  <MenuItem key={f} onClick={() => exportAs(f)}>
                    {{ dotenv: ".env", json: "json", shell: "shell", yaml: "yaml", docker: "Docker env file", kubernetes: "Kubernetes Secret" }[f]} <span className="ml-auto text-[10.5px] text-subtle">audited</span>
                  </MenuItem>
                ))}
              </MenuPopup>
            </Menu>
            <button className={button} onClick={() => setDrawer({ kind: "changes" })} title="History (h)">
              <History className="size-3.5" /> History
            </button>
          </span>
        </header>

        <div className="flex flex-wrap items-center gap-2 border-y border-line px-3 py-2">
          <Segmented
            label="Show"
            value={tab}
            onChange={(t) => go({ env: env.slug, path: p, tab: t === "secrets" ? undefined : t }, true)}
            options={[
              { value: "secrets", label: <span className="px-1.5 text-[11.5px]">Secrets</span> },
              {
                value: "requests",
                label: (
                  <span className="flex items-center gap-1 px-1.5 text-[11.5px]">
                    Requests
                    {!!open.data?.length && <span className="grid h-4 min-w-4 place-items-center rounded-full bg-accent/15 px-1 font-mono text-[10px] text-accent">{open.data.length}</span>}
                  </span>
                ),
              },
              { value: "activity", label: <span className="px-1.5 text-[11.5px]">Activity</span> },
            ]}
          />
          {tab === "secrets" && (
            <>
              <FolderCrumbs path={path} onPath={(np) => go({ env: env.slug, path: np === "/" ? undefined : np })} />
              {childFolders.map((f) => (
                <button key={f.path} onClick={() => go({ env: env.slug, path: f.path })} className="inline-flex h-6 items-center gap-1 rounded-md border border-line-2 px-1.5 text-[11.5px] text-muted transition-colors hover:border-line-3 hover:text-fg" title={`${f.secrets} secrets, ${f.folders} folders`}>
                  <Folder className="size-3" /> {f.name || folderName(f.path)}
                  <span className="font-mono text-[10px] text-subtle">{f.secrets}</span>
                </button>
              ))}
              <button
                onClick={() => setImportsOpen((o) => (o ? null : { highlight: null }))}
                aria-expanded={!!importsOpen}
                className={`inline-flex h-6 items-center gap-1 rounded-md px-1.5 text-[11.5px] transition-colors hover:bg-fg/[0.05] hover:text-fg ${importsOpen ? "bg-fg/[0.05] text-fg" : "text-muted"}`}
                title="Folders whose keys show up here"
              >
                <ImportIcon className="size-3" /> Imports
                {!!imports.data?.length && <span className="font-mono text-[10px] text-subtle">{imports.data.length}</span>}
              </button>
              <NewFolder path={path} onCreate={(np) => api.createFolder(env.slug, np).then(() => go({ env: env.slug, path: np }), (e) => setError(errorText(e)))} />
              <span className="ml-auto" />
              {insights.data && (unreadCount > 0 || unread) && (
                <Chip on={unread} onClick={() => setUnread((u) => !u)} title="Keys nobody revealed, exported, resolved or synced in 90 days: candidates to clean up">
                  Not read in 90 days <span className="font-mono text-[10px] text-subtle">{unreadCount}</span>
                </Chip>
              )}
              <label className="flex h-7 w-52 items-center gap-1.5 rounded-md border border-line-2 bg-panel-2 px-2 text-subtle focus-within:border-accent/60 max-sm:w-full">
                <Search className="size-3.5" />
                <input ref={filter} value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Filter" aria-label="Filter secrets" className="min-w-0 flex-1 bg-transparent font-mono text-[12px] text-fg outline-none placeholder:font-sans placeholder:text-subtle" />
                <kbd className="font-mono text-[10px]">/</kbd>
              </label>
            </>
          )}
        </div>

        <div className="space-y-2 px-3 pt-3 empty:hidden">
          <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} />
          {pending && (
            <ApprovalOffer
              env={env}
              pending={pending}
              onCancel={() => setPending(null)}
              onOpened={(r) => {
                setPending(null);
                setNote({ text: `Opened change request ${r.id}`, warnings: [] });
                open.reload();
              }}
            />
          )}
          {note && (
            <div className="fade-in flex items-start gap-2 text-[12px] text-muted" role="status">
              <span className="min-w-0 flex-1">
                {note.text}
                <Warnings items={note.warnings} className="mt-1.5" />
              </span>
              <button onClick={() => setNote(null)} className="text-subtle hover:text-fg" aria-label="Dismiss">
                ×
              </button>
            </div>
          )}
        </div>

        {tab === "secrets" && importsOpen && imports.data && (
          <ImportsPanel
            env={env}
            path={path}
            saved={imports.data}
            highlight={importsOpen.highlight}
            onClose={() => setImportsOpen(null)}
            onSaved={(list) => {
              imports.set(list);
              setNote({ text: list.length ? `Imports saved: ${list.map((i) => `${i.environment}${i.path === "/" ? "" : i.path}`).join(", ")}` : "This folder imports nothing now", warnings: [] });
              refresh();
            }}
          />
        )}
        {tab === "activity" ? (
          <Activity env={env} />
        ) : tab === "requests" ? (
          <RequestsPanel env={env} onChanged={() => (open.reload(), refresh())} />
        ) : (
          <div className="px-3 pt-2 pb-8">
            {adding && (
              <AddSecret
                path={path}
                keys={keys}
                refs={refSource}
                onCancel={() => setAdding(false)}
                onAdd={async (op) => {
                  const batch: Batch = { path, ops: [op], message: `added ${op.key}` };
                  const ok = await write(() => api.set(env.slug, op.key, { value: op.op === "set" ? op.value : undefined, comment: op.op === "set" ? op.comment : undefined, tags: op.op === "set" ? op.tags : undefined, type: op.type }, path), batch, `Added ${op.key}`);
                  if (ok) setAdding(false);
                  return ok;
                }}
              />
            )}
            {list.data && secrets.length === 0 && !adding ? (
              <Teach
                title={path === "/" ? `${env.name} has no secrets yet` : `${path} is empty in ${env.name}`}
                cli={[`roll secrets set -p ${project.name} -e ${env.slug} KEY=value`, `roll run -p ${project.name} --env ${env.slug} -- <cmd>`]}
              >
                Add one, paste a .env file, or link a resource so its connection string and credentials stay correct when it changes.
                {standard.length > 1 && " Compare promotes keys from another environment."} From the CLI:
              </Teach>
            ) : (
              <div role="table" aria-label={`Secrets in ${env.name}`} aria-multiselectable className="@container text-[12.5px]">
                {picked.length > 0 && (
                  <BulkBar
                    env={env}
                    path={path}
                    picked={picked}
                    onWrite={write}
                    onNote={(text) => setNote({ text, warnings: [] })}
                    onClear={() => setSelected(new Set())}
                    onDone={() => {
                      setSelected(new Set());
                      refresh();
                    }}
                  />
                )}
                {selectable.length > 1 && (
                  <label className="flex h-7 items-center gap-2 px-2 text-[11px] text-subtle">
                    <input
                      type="checkbox"
                      checked={selectable.every((s) => selected.has(rowId(s)))}
                      ref={(el) => {
                        if (el) el.indeterminate = picked.length > 0 && !selectable.every((s) => selected.has(rowId(s)));
                      }}
                      onChange={(e) => setSelected(e.target.checked ? new Set(selectable.map(rowId)) : new Set())}
                      aria-label="Select all"
                      className="accent-[var(--accent)]"
                    />
                    {picked.length ? `${picked.length} of ${selectable.length}` : "Select"}
                    <span className="text-faint max-sm:hidden">· shift-click a range</span>
                  </label>
                )}
                {shown.map((s) => (
                  <SecretRow
                    key={`${s.key}:${s.type}`}
                    env={env}
                    secret={s}
                    path={path}
                    keys={keys}
                    refs={refSource}
                    insight={insightOf.get(s.key)}
                    maxAge={insights.data?.max_age_days ?? env.max_age_days}
                    selected={selected.has(rowId(s))}
                    onSelect={s.source.kind === "own" ? (on, range) => select(s, on, range) : undefined}
                    focus={search.key === s.key}
                    onWrite={write}
                    onHistory={() => setDrawer({ kind: "versions", key: s.key, type: s.type })}
                    onShare={() => setSharing({ kind: "secret", env: env.slug, key: s.key, path })}
                    onSource={(src) => (src.kind === "import" ? setImportsOpen({ highlight: { ...(src.project && { project: src.project }), environment: src.environment, path: src.path } }) : go({ env: src.environment, path: p, key: s.key }))}
                  />
                ))}
                {list.data && shown.length === 0 && secrets.length > 0 && <p className="px-2 py-6 text-center text-[12px] text-subtle">{unread && !query ? "Every key here was read in the last 90 days." : `Nothing matches "${query}".`}</p>}
              </div>
            )}
          </div>
        )}
      </div>

      <HistoryDrawer env={env} drawer={drawer} onClose={() => setDrawer(null)} onDrawer={setDrawer} onChanged={refresh} />
      <ImportDialog open={importing} onClose={() => setImporting(false)} env={env} path={path} current={secrets} onImported={(text, w) => (setNote({ text, warnings: w }), refresh(), open.reload())} />
      <ShareDialog source={sharing} onClose={() => setSharing(null)} onShared={() => {}} />
      <LinkDialog open={linking} onClose={() => setLinking(false)} env={env} path={path} taken={keys} onWrite={write} />
    </div>
  );
}

function NewFolder({ path, onCreate }: { path: string; onCreate: (path: string) => void }) {
  const [name, setName] = useState<string | null>(null);
  if (name === null)
    return (
      <button onClick={() => setName("")} className={iconButton} title="New folder" aria-label="New folder">
        <FolderPlus className="size-3.5" />
      </button>
    );
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name);
  return (
    <form
      className="flex items-center gap-1"
      onSubmit={(e) => {
        e.preventDefault();
        if (ok) onCreate(childPath(path, name));
        setName(null);
      }}
    >
      <input
        autoFocus
        value={name}
        onChange={(e) => setName(e.target.value)}
        onKeyDown={(e) => e.key === "Escape" && setName(null)}
        onBlur={() => !name && setName(null)}
        placeholder="folder"
        aria-label="Folder name"
        className={`${sized} h-6 w-28 font-mono text-[11.5px]`}
      />
    </form>
  );
}

const parseTags = (s: string) => s.split(/[\s,]+/).filter(Boolean);

function AddSecret({ path, keys, refs, onAdd, onCancel }: { path: string; keys: Set<string>; refs: RefSource; onAdd: (op: Op) => Promise<boolean>; onCancel: () => void }) {
  const { required } = useSecrets();
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const [comment, setComment] = useState("");
  const [tags, setTags] = useState("");
  const [personal, setPersonal] = useState(false);
  const [busy, setBusy] = useState(false);
  const keyOk = SECRET_KEY.test(key);
  const tagList = parseTags(tags);
  const tagsOk = tagList.every((t) => TAG.test(t));
  const warn = useRefWarnings(value, key, refs);
  const exists = keys.has(key) && !personal;
  const unfit = misfit(required.find((r) => r.key === key && r.path === path), value);
  const submit = async () => {
    if (!keyOk || !tagsOk || unfit || busy) return;
    setBusy(true);
    const ok = await onAdd({ op: "set", key, value, comment: comment || undefined, tags: tagList.length ? tagList : undefined, type: personal ? "personal" : undefined });
    setBusy(false);
    if (ok) setValue("");
  };
  return (
    <form
      className="rise mb-3 rounded-xl border border-line-2 bg-panel p-3 shadow-card inset-shadow-highlight"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape") onCancel();
        if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) void submit();
      }}
    >
      <div className="grid gap-2 sm:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <label>
          <span className={label}>Key</span>
          <input autoFocus value={key} onChange={(e) => setKey(e.target.value.replace(/[\s-]/g, "_"))} placeholder="DATABASE_URL" spellCheck={false} className={`${field} font-mono`} />
          {key && !keyOk && <span className="mt-1 block text-[11px] text-danger">A letter or _ then letters, digits and _.</span>}
          {exists && <span className="mt-1 block text-[11px] text-muted">{key} exists: this writes a new version.</span>}
        </label>
        <div>
          <ValueHead onGenerate={() => setValue(generateValue())} />
          <RefInput aria-label="Value" value={value} onValue={setValue} source={refs} rows={value.includes("\n") ? 4 : 1} placeholder="value, or ${OTHER_KEY}, ${production.KEY}" spellCheck={false} autoComplete="off" className={`${field} h-auto min-h-8 resize-y py-1.5 font-mono`} />
          {unfit && value && <span className="mt-1 block text-[11px] text-danger">{unfit}</span>}
        </div>
        <label>
          <span className={label}>Comment</span>
          <input value={comment} onChange={(e) => setComment(e.target.value)} maxLength={1000} placeholder="What it's for" className={field} />
        </label>
        <label>
          <span className={label}>Tags</span>
          <input value={tags} onChange={(e) => setTags(e.target.value.toLowerCase())} placeholder="billing, rotate-yearly" className={`${field} font-mono`} />
          {!tagsOk && <span className="mt-1 block text-[11px] text-danger">Tags are a-z, 0-9 and -, up to 32 characters.</span>}
        </label>
      </div>
      <Warnings items={warn} className="mt-2" />
      <div className="mt-3 flex items-center gap-2">
        <label className="flex items-center gap-1.5 text-[12px] text-muted" title="Only you see a personal value; it replaces the shared one when you read this environment">
          <input type="checkbox" checked={personal} onChange={(e) => setPersonal(e.target.checked)} className="accent-[var(--accent)]" />
          <User className="size-3.5" /> Personal override
        </label>
        <span className="ml-auto text-[11px] text-subtle max-sm:hidden">⌘↵ to add · Esc to close</span>
        <button type="button" onClick={onCancel} className={ghost}>
          Cancel
        </button>
        <button type="submit" disabled={!keyOk || !tagsOk || !!unfit || busy} className={primary}>
          {busy ? "Adding…" : "Add secret"}
        </button>
      </div>
    </form>
  );
}

type EditMode = "edit" | "personal" | "own";

type WriteFn = (run: () => Promise<{ warnings?: string[]; change?: { seq: number } | null }>, batch: Batch, done: string) => Promise<boolean>;

function SecretRow({
  env,
  secret: s,
  path,
  keys,
  refs,
  insight,
  maxAge,
  selected,
  onSelect,
  focus,
  onWrite,
  onHistory,
  onShare,
  onSource,
}: {
  env: Environment;
  secret: Secret;
  path: string;
  keys: Set<string>;
  refs: RefSource;
  insight: KeyInsight | undefined;
  maxAge: number | null;
  selected: boolean;
  /** Imported and inherited rows aren't selectable. */
  onSelect?: (on: boolean, range: boolean) => void;
  focus: boolean;
  onWrite: WriteFn;
  onHistory: () => void;
  onShare: () => void;
  onSource: (source: Exclude<Secret["source"], { kind: "own" }>) => void;
}) {
  const { api, project, resourceName } = useSecrets();
  const [editing, setEditing] = useState<null | EditMode>(null);
  const [confirming, setConfirming] = useState(false);
  const row = useRef<HTMLDivElement>(null);
  const own = s.source.kind === "own";
  const personal = s.type === "personal";

  useEffect(() => {
    if (focus) row.current?.scrollIntoView({ block: "center" });
  }, [focus]);

  const del = () =>
    onWrite(() => api.remove(env.slug, s.key, path, personal ? "personal" : undefined), { path, ops: [{ op: "delete", key: s.key, type: personal ? "personal" : undefined }], message: `deleted ${s.key}` }, `Deleted ${s.key}${personal ? " (your override)" : ""}`).then(() => setConfirming(false));
  const rotate = () => onWrite(() => api.rotate(env.slug, s.key, path), { path, ops: [] }, `Rotated ${s.key}`);

  return (
    <div ref={row} role="row" aria-selected={onSelect ? selected : undefined} className={`group/row border-t border-line first:border-t-0 ${selected ? "bg-accent/[0.06]" : focus ? "bg-accent/[0.05]" : ""}`}>
      <div className="grid min-h-9 grid-cols-[minmax(0,1.1fr)_minmax(0,1.6fr)_minmax(0,1fr)_minmax(6.5rem,auto)] items-center gap-3 px-2 py-1 @max-4xl:grid-cols-[minmax(0,1fr)_minmax(0,1.6fr)_minmax(6.5rem,auto)] @max-xl:grid-cols-[minmax(0,1fr)_minmax(6.5rem,auto)] @max-xl:gap-y-0.5">
        <div role="cell" className="flex min-w-0 items-center gap-1.5">
          <input
            type="checkbox"
            checked={selected}
            disabled={!onSelect}
            onChange={() => {}}
            onClick={(e) => onSelect?.(!selected, e.shiftKey)}
            aria-label={`Select ${s.key}${personal ? " (your override)" : ""}`}
            title={onSelect ? undefined : "Read-only here: it comes from elsewhere"}
            className={`shrink-0 accent-[var(--accent)] transition-opacity ${selected ? "" : "opacity-40 group-hover/row:opacity-100 focus-visible:opacity-100"} disabled:opacity-0`}
          />
          <span className={`max-w-[70%] shrink-0 truncate font-mono text-[12px] ${own ? "text-fg" : "text-muted"}`} title={s.key}>
            {s.key}
          </span>
          {personal && <PersonalMark title="Your personal value: only you see it" />}
          {s.overridden && !personal && (
            <span className="text-[10.5px] text-subtle" title="Your personal override replaces this for you">
              overridden for you
            </span>
          )}
          {insight?.stale && own && (
            <span className="inline-flex shrink-0 text-danger" title={`Unchanged since ${ago(insight.updated_at)}: longer than this environment's ${maxAge ?? "?"}-day limit`} aria-label="Stale">
              <Clock className="size-3" />
            </span>
          )}
          {s.references.length > 0 && <RefMark refs={s.references} />}
          <SourceMark source={s.source} onOpen={s.source.kind === "own" ? undefined : () => s.source.kind !== "own" && onSource(s.source)} />
        </div>
        <div role="cell" className="flex min-w-0 items-center gap-1.5 @max-xl:order-last @max-xl:col-span-2">
          {s.link && <LinkChip link={s.link} name={resourceName(s.link)} />}
          {s.link?.mode === "dynamic" ? (
            <span className="text-[11px] text-subtle" title="Each resolve (roll run, an app's start) issues a fresh credential with a lease">
              issued per resolve ·{" "}
              <Link className="text-accent hover:underline" href={secretsHref(project.id, { view: "access" })}>
                leases
              </Link>
            </span>
          ) : (
            <Reveal className="flex-1" load={() => api.value(env.slug, s.key, path)} />
          )}
        </div>
        <div role="cell" className="flex min-w-0 items-center gap-1.5 text-[11.5px] text-muted @max-4xl:hidden">
          {s.comment && (
            <span className="truncate" title={s.comment}>
              {s.comment}
            </span>
          )}
          {s.tags.map((t) => (
            <span key={t} className="shrink-0 rounded border border-line-2 px-1 font-mono text-[10px] text-subtle">
              {t}
            </span>
          ))}
          {s.expires_at != null && <ExpiryMark at={s.expires_at} />}
          {s.weak && (
            <span className="shrink-0 rounded border border-danger/40 px-1 font-mono text-[10px] text-danger" title={`Easy to guess: ${s.weak}. Generate one (roll secrets set ${s.key} --generate 48).`}>
              weak
            </span>
          )}
          {!!insight?.exposed_in.length && (
            <span className="shrink-0 rounded border border-danger/40 px-1 font-mono text-[10px] text-danger" title={`The same value is set in ${insight.exposed_in.join(", ")}, which isn't protected: whoever reads it there has this protected value. Give each its own value.`}>
              also in {insight.exposed_in.join(", ")}
            </span>
          )}
          {!s.comment && !s.tags.length && <span className="truncate text-faint" title={`by ${s.updated_by}`}>v{s.version} · {ago(s.updated_at)}</span>}
          {insight && (
            <span
              className={`ml-auto shrink-0 pl-2 text-[11px] ${insight.last_read_at ? "text-faint" : "text-subtle"}`}
              title={insight.last_read_at ? `Last read by ${insight.last_read_by ?? "?"} · ${insight.reads_30d} ${insight.reads_30d === 1 ? "read" : "reads"} in 30 days` : "Nobody revealed, exported, resolved or synced it in 90 days"}
            >
              {insight.last_read_at ? `read ${ago(insight.last_read_at)}` : "never in 90 days"}
            </span>
          )}
        </div>
        <div role="cell" className="flex items-center justify-end gap-0.5">
          {confirming ? (
            <span className="fade-in flex items-center gap-1">
              <button onClick={del} className={dangerButton}>
                Delete {personal ? "override" : s.key.length > 16 ? "" : s.key}
              </button>
              <button onClick={() => setConfirming(false)} className={ghost}>
                Keep
              </button>
            </span>
          ) : (
            <span className="flex items-center gap-0.5 opacity-60 transition-opacity group-hover/row:opacity-100 focus-within:opacity-100">
              {s.comment && <CommentMark text={s.comment} />}
              {own && (
                <button className={iconButton} onClick={() => setEditing(editing ? null : "edit")} aria-label={`Edit ${s.key}`} title="Edit">
                  <Pencil className="size-3.5" />
                </button>
              )}
              <button className={iconButton} onClick={onHistory} aria-label={`History of ${s.key}`} title="Versions">
                <History className="size-3.5" />
              </button>
              <Menu>
                <MenuTrigger className={iconButton} aria-label={`More for ${s.key}`}>
                  <MoreHorizontal className="size-3.5" />
                </MenuTrigger>
                <MenuPopup align="end" className="w-56">
                  {!personal && (
                    <MenuItem onClick={() => setEditing("personal")}>
                      <User className="size-3.5" /> Override for me
                    </MenuItem>
                  )}
                  {!own && (
                    <MenuItem onClick={() => setEditing("own")}>
                      <Pencil className="size-3.5" /> Set here (own value)
                    </MenuItem>
                  )}
                  {s.link?.mode !== "dynamic" && (
                    <MenuItem onClick={onShare}>
                      <Send className="size-3.5" /> Share once…
                    </MenuItem>
                  )}
                  {s.link && canRotate(s.link) && own && (
                    <MenuItem onClick={rotate}>
                      <RotateCw className="size-3.5" /> Rotate {s.link.mode === "managed" ? "credential" : "the resource's password"}
                    </MenuItem>
                  )}
                  {own && (
                    <MenuItem variant="destructive" onClick={() => setConfirming(true)}>
                      <Trash2 className="size-3.5" /> Delete{personal ? " my override" : ""}
                    </MenuItem>
                  )}
                </MenuPopup>
              </Menu>
            </span>
          )}
        </div>
      </div>
      {editing && (
        <Editor
          secret={s}
          refs={refs}
          mode={editing}
          onCancel={() => setEditing(null)}
          onSave={async (body) => {
            const type: SecretType | undefined = editing === "personal" || (editing === "edit" && personal) ? "personal" : undefined;
            const op: Op = { op: "set", key: s.key, ...body, type };
            const ok = await onWrite(() => api.set(env.slug, s.key, { ...body, type }, path), { path, ops: [op], message: `updated ${s.key}` }, editing === "personal" ? `Set ${s.key} for you` : editing === "own" ? `Set ${s.key} here` : `Saved ${s.key}`);
            if (ok) setEditing(null);
          }}
        />
      )}
    </div>
  );
}

/** Inline edit: a new value (blank keeps the current one), the comment and the tags; a linked secret can be unlinked. */
function Editor({
  secret: s,
  refs,
  mode,
  onSave,
  onCancel,
}: {
  secret: Secret;
  refs: RefSource;
  mode: EditMode;
  onSave: (body: { value?: string; comment?: string; tags?: string[]; link?: null; expires_at?: number | null }) => Promise<void>;
  onCancel: () => void;
}) {
  const fresh = mode !== "edit";
  const [value, setValue] = useState("");
  const [touched, setTouched] = useState(fresh);
  const [comment, setComment] = useState(fresh ? "" : s.comment);
  const [tags, setTags] = useState(fresh ? "" : s.tags.join(", "));
  const day = (ms: number | null) => (ms == null ? "" : new Date(ms).toISOString().slice(0, 10));
  const [expires, setExpires] = useState(fresh ? "" : day(s.expires_at));
  const [unlink, setUnlink] = useState(false);
  const [busy, setBusy] = useState(false);
  const linked = !!s.link && !unlink && !fresh;
  const tagList = parseTags(tags);
  const tagsOk = tagList.every((t) => TAG.test(t));
  const warn = useRefWarnings(value, s.key, refs, touched && !linked);
  const { required } = useSecrets();
  const unfit = touched && !linked ? misfit(required.find((r) => r.key === s.key && r.path === s.path), value) : null;
  const save = async () => {
    if (!tagsOk || unfit || busy) return;
    setBusy(true);
    await onSave({
      ...(touched && !linked && { value }),
      ...(comment !== s.comment || fresh ? { comment } : {}),
      ...(tags !== s.tags.join(", ") || fresh ? { tags: tagList } : {}),
      ...(unlink && { link: null }),
      ...(expires !== (fresh ? "" : day(s.expires_at)) ? { expires_at: expires ? Date.parse(`${expires}T00:00:00Z`) : null } : {}),
    });
    setBusy(false);
  };
  return (
    <form
      className="fade-in mx-2 mb-2 rounded-lg border border-line-2 bg-panel p-2.5 inset-shadow-highlight"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape") onCancel();
        if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) void save();
      }}
    >
      {fresh && (
        <p className="mb-2 text-[11.5px] text-muted">
          {mode === "personal" ? (
            <>
              <User className="mr-1 inline size-3" />A personal value for <b className="font-mono font-medium text-fg">{s.key}</b>: only you see it, and it replaces the shared value when you read this environment.
            </>
          ) : (
            <>This folder's own value for {s.key}, in place of the one it gets from elsewhere.</>
          )}
        </p>
      )}
      <div className="grid gap-2 sm:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <div>
          <ValueHead onGenerate={linked ? undefined : () => (setValue(generateValue()), setTouched(true))} />
          {linked ? (
            <div className="flex h-8 items-center gap-2 text-[11.5px] text-muted">
              The value comes from the link.
              <button type="button" className="text-accent hover:underline" onClick={() => setUnlink(true)}>
                Unlink and set a value
              </button>
            </div>
          ) : (
            <RefInput
              aria-label="Value"
              autoFocus
              value={value}
              source={refs}
              onValue={(v) => {
                setValue(v);
                setTouched(true);
              }}
              rows={value.includes("\n") ? 4 : 1}
              placeholder={fresh ? "value" : "unchanged: type a new value to replace it"}
              spellCheck={false}
              autoComplete="off"
              className={`${field} h-auto min-h-8 resize-y py-1.5 font-mono`}
            />
          )}
          {unfit && <span className="mt-1 block text-[11px] text-danger">{unfit}</span>}
        </div>
        <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] gap-2">
          <label>
            <span className={label}>Comment</span>
            <input value={comment} onChange={(e) => setComment(e.target.value)} maxLength={1000} className={field} />
          </label>
          <label>
            <span className={label}>Tags</span>
            <input value={tags} onChange={(e) => setTags(e.target.value.toLowerCase())} className={`${field} font-mono`} />
          </label>
          <label title="When the value stops working outside roll (a vendor's key, a certificate): flagged in insights, announced two weeks ahead">
            <span className={label}>Expires</span>
            <input type="date" value={expires} onChange={(e) => setExpires(e.target.value)} className={`${field} w-34 font-mono`} />
          </label>
        </div>
      </div>
      {!tagsOk && <p className="mt-1 text-[11px] text-danger">Tags are a-z, 0-9 and -, up to 32 characters.</p>}
      <Warnings items={warn} className="mt-2" />
      <div className="mt-2 flex items-center justify-end gap-2">
        <span className="mr-auto text-[11px] text-subtle max-sm:hidden">⌘↵ to save · Esc to cancel</span>
        <button type="button" onClick={onCancel} className={ghost}>
          Cancel
        </button>
        <button type="submit" disabled={!tagsOk || !!unfit || busy || (unlink && !touched)} className={primary}>
          {busy ? "Saving…" : "Save"}
        </button>
      </div>
    </form>
  );
}

/** A form's "Value" label, with a button that fills in a random value (48 letters and digits). */
function ValueHead({ onGenerate }: { onGenerate?: () => void }) {
  return (
    <span className={`${label} flex items-center justify-between`}>
      Value
      {onGenerate && (
        <button type="button" onClick={onGenerate} className="flex items-center gap-1 text-[11px] font-normal text-muted transition-colors hover:text-accent" title="A random value of 48 letters and digits, made here in the browser">
          <Dices className="size-3" /> Generate
        </button>
      )}
    </span>
  );
}
