import { Check } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { useState, useSyncExternalStore } from "react";
import { DESCRIPTION_MAX, PROJECT_NAME, type ResourceKind, type WorkspaceIcon } from "@/lib/workspace-api";
import { CREATABLE, KIND_BLURB, KIND_LABEL, resourceKey, suggestName } from "@/lib/workspace-resources";
import { useWorkspace, workspace } from "@/lib/workspace-store";
import { Dialog } from "../agents/ui/dialog";
import { KindTile } from "./kind";
import { IconEditor } from "./project-icon";

// Whether the dialog is open, from anywhere (the sidebar's +, Home, an empty sidebar).
let open = false;
const listeners = new Set<() => void>();
const setOpen = (v: boolean) => {
  open = v;
  for (const l of listeners) l();
};
export const openNewProject = () => setOpen(true);
const useOpen = () =>
  useSyncExternalStore(
    (l) => (listeners.add(l), () => listeners.delete(l)),
    () => open,
    () => false,
  );

/** "New project": a name, how it looks, and what it starts with (staged on its canvas, made on Deploy). */
export function NewProjectDialog() {
  const shown = useOpen();
  const [key, setKey] = useState(0);
  return (
    <Dialog
      open={shown}
      onClose={() => setOpen(false)}
      onExited={() => setKey((k) => k + 1)}
      title="New project"
      lead="One thing you build: its databases, storage, repositories and agents, on one canvas."
      className="w-[min(560px,100%)]"
    >
      <Form key={key} onDone={() => setOpen(false)} />
    </Dialog>
  );
}

const slug = (s: string) =>
  s
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^-+/, "")
    .slice(0, 48);

function Form({ onDone }: { onDone: () => void }) {
  const ws = useWorkspace();
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [icon, setIcon] = useState<WorkspaceIcon | null>(null);
  const [kinds, setKinds] = useState<Set<ResourceKind>>(new Set());
  const [repo, setRepo] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const placed = new Set(ws.projects.flatMap((p) => p.resources.map(resourceKey)));
  const repos = [...ws.inventory.values()].filter((v) => v.kind === "repo" && !placed.has(resourceKey(v)));
  const offered = CREATABLE.filter((k) => ws.kinds.has(k));
  const clean = name.replace(/-+$/, "");
  const valid = PROJECT_NAME.test(clean);
  const taken = ws.projects.some((p) => p.name === clean);

  const submit = async () => {
    if (!valid || taken || busy) return;
    setBusy(true);
    setError(null);
    try {
      const p = await workspace.createProject(clean, { description: description.trim() || undefined, icon });
      const names = new Set([...ws.inventory.values()].map((v) => v.name));
      if (repo) await workspace.place(p.id, "repo", repo, 48, 48);
      for (const k of offered.filter((k) => kinds.has(k))) names.add(workspace.stage(p.id, k, suggestName(k, p.name, names)).name);
      onDone();
      navigate({ href: `/dashboard/projects/${p.id}` });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  return (
    <form
      className="space-y-4"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <IconEditor project={{ id: "", name: clean || "project" }} value={icon} onChange={setIcon} />

      <label className="block">
        <span className="mb-1.5 block text-[11.5px] font-medium text-subtle">Name</span>
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(slug(e.target.value))}
          placeholder="my-app"
          className="h-9 w-full rounded-lg border border-line-2 bg-canvas px-3 font-mono text-[13px] outline-none transition-colors focus:border-accent/60"
        />
        <span className={`mt-1 block text-[11px] ${taken ? "text-danger" : "text-subtle"}`}>
          {taken ? `You have a project named ${clean}.` : "Lowercase letters, digits and dashes."}
        </span>
      </label>

      <label className="block">
        <span className="mb-1.5 block text-[11.5px] font-medium text-subtle">Description</span>
        <textarea
          value={description}
          maxLength={DESCRIPTION_MAX}
          rows={2}
          onChange={(e) => setDescription(e.target.value)}
          placeholder="What it is, for whoever opens it next (optional)"
          className="w-full resize-none rounded-lg border border-line-2 bg-canvas px-3 py-2 text-[12.5px] outline-none transition-colors focus:border-accent/60"
        />
      </label>

      {offered.length > 0 && (
        <div>
          <span className="mb-1.5 block text-[11.5px] font-medium text-subtle">Start with</span>
          <div className="grid grid-cols-2 gap-1.5 sm:grid-cols-3">
            {offered.map((k) => {
              const on = kinds.has(k);
              return (
                <button
                  key={k}
                  type="button"
                  aria-pressed={on}
                  onClick={() => setKinds((s) => (s.has(k) ? new Set([...s].filter((x) => x !== k)) : new Set([...s, k])))}
                  className={`flex items-center gap-2 rounded-lg border px-2 py-1.5 text-left transition-[border-color,background-color] duration-150 ${
                    on ? "border-accent/60 bg-accent/[0.06]" : "border-line-2 bg-raised hover:border-line-3"
                  }`}
                >
                  <KindTile kind={k} size={22} />
                  <span className="min-w-0 flex-1 leading-tight">
                    <span className="block text-[12px] font-medium">{KIND_LABEL[k]}</span>
                    <span className="block truncate text-[10.5px] text-subtle">{KIND_BLURB[k]}</span>
                  </span>
                  <Check className={`size-3.5 text-accent transition-opacity duration-150 ${on ? "opacity-100" : "opacity-0"}`} />
                </button>
              );
            })}
          </div>
          <p className="mt-1.5 text-[11px] text-subtle">They wait on the canvas until you deploy. Everything scales to zero when idle.</p>
        </div>
      )}

      {repos.length > 0 && (
        <label className="block">
          <span className="mb-1.5 block text-[11.5px] font-medium text-subtle">Repository</span>
          <select
            value={repo}
            onChange={(e) => setRepo(e.target.value)}
            className="h-9 w-full rounded-lg border border-line-2 bg-canvas px-2.5 text-[12.5px] outline-none focus:border-accent/60"
          >
            <option value="">None for now</option>
            {repos.map((r) => (
              <option key={r.ref} value={r.ref}>
                {r.ref}
              </option>
            ))}
          </select>
          <span className="mt-1 block text-[11px] text-subtle">Agents working in it show up in this project.</span>
        </label>
      )}

      {error && <p className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">{error}</p>}

      <div className="flex justify-end gap-2 border-t border-line pt-3">
        <button type="button" onClick={onDone} className="h-8 rounded-lg px-3 text-[12.5px] text-muted hover:bg-white/[0.05] hover:text-fg">
          Cancel
        </button>
        <button
          type="submit"
          disabled={!valid || taken || busy}
          className="h-8 rounded-lg bg-fg px-3.5 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40"
        >
          {busy ? "Creating…" : "Create project"}
        </button>
      </div>
    </form>
  );
}
