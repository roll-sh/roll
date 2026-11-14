import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { DESCRIPTION_MAX, PROJECT_NAME, type WorkspaceIcon, type WorkspaceProject } from "@/lib/workspace-api";
import { workspace } from "@/lib/workspace-store";
import { Dialog } from "../agents/ui/dialog";
import { ProjectApps } from "../apps/project-apps";
import { IconEditor } from "./project-icon";

const field = "w-full rounded-lg border border-line-2 bg-canvas px-3 outline-none transition-colors focus:border-accent/60";

/** A project's settings: how it looks, its name and description, and deleting it; with services,
 *  what roll apps keeps for all of them (notifications, log drains, spending limits). */
export function ProjectSettings({ project, open, onClose }: { project: WorkspaceProject; open: boolean; onClose: () => void }) {
  const [key, setKey] = useState(0);
  const [tab, setTab] = useState<"general" | "apps">("general");
  const apps = project.resources.some((r) => r.kind === "service");
  const shown = apps ? tab : "general";
  return (
    <Dialog
      open={open}
      onClose={onClose}
      onExited={() => {
        setKey((k) => k + 1);
        setTab("general");
      }}
      title="Project settings"
      className={shown === "apps" ? "w-[min(820px,100%)]" : "w-[min(560px,100%)]"}
    >
      {apps && (
        <div className="-mt-1 mb-4 flex gap-1 border-b border-line pb-2" role="tablist" aria-label="Project settings">
          {(["general", "apps"] as const).map((t) => (
            <button
              key={t}
              id={`project-tab-${t}`}
              role="tab"
              aria-selected={shown === t}
              aria-controls="project-tabpanel"
              tabIndex={shown === t ? 0 : -1}
              onClick={() => setTab(t)}
              onKeyDown={(e) => {
                if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
                e.preventDefault();
                const next = t === "general" ? "apps" : "general";
                setTab(next);
                document.getElementById(`project-tab-${next}`)?.focus();
              }}
              className={`h-7 rounded-md px-2.5 text-[12px] transition-colors duration-150 ${shown === t ? "bg-fg/[0.07] text-fg" : "text-muted hover:bg-fg/[0.04] hover:text-fg"}`}
            >
              {t === "general" ? "General" : "Services"}
            </button>
          ))}
        </div>
      )}
      <div id="project-tabpanel" role={apps ? "tabpanel" : undefined} aria-labelledby={apps ? `project-tab-${shown}` : undefined}>
        {shown === "apps" ? <ProjectApps key={key} project={project.id} projectName={project.name} /> : <Form key={key} project={project} onDone={onClose} />}
      </div>
    </Dialog>
  );
}

function Form({ project, onDone }: { project: WorkspaceProject; onDone: () => void }) {
  const navigate = useNavigate();
  const [icon, setIcon] = useState<WorkspaceIcon | null>(project.icon);
  const [name, setName] = useState(project.name);
  const [description, setDescription] = useState(project.description);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState("");

  const changed = name !== project.name || description !== project.description || JSON.stringify(icon) !== JSON.stringify(project.icon);
  const valid = PROJECT_NAME.test(name);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await workspace.update(project.id, {
        ...(name !== project.name && { name }),
        ...(description !== project.description && { description }),
        ...(JSON.stringify(icon) !== JSON.stringify(project.icon) && { icon }),
      });
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-4">
      <form
        className="space-y-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (changed && valid && !busy) void save();
        }}
      >
        <IconEditor project={{ id: project.id, name }} value={icon} onChange={setIcon} />
        <label className="block">
          <span className="mb-1.5 block text-[11.5px] font-medium text-subtle">Name</span>
          <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} className={`${field} h-9 font-mono text-[13px]`} />
          {!valid && <span className="mt-1 block text-[11px] text-danger">Lowercase letters, digits and dashes, not starting or ending with one.</span>}
        </label>
        <label className="block">
          <span className="mb-1.5 block text-[11.5px] font-medium text-subtle">Description</span>
          <textarea value={description} maxLength={DESCRIPTION_MAX} rows={2} onChange={(e) => setDescription(e.target.value)} className={`${field} resize-none py-2 text-[12.5px]`} />
        </label>
        {error && <p className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">{error}</p>}
        <div className="flex justify-end gap-2">
          <button type="button" onClick={onDone} className="h-8 rounded-lg px-3 text-[12.5px] text-muted hover:bg-white/[0.05] hover:text-fg">
            Cancel
          </button>
          <button type="submit" disabled={!changed || !valid || busy} className="h-8 rounded-lg bg-fg px-3.5 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40">
            {busy ? "Saving…" : "Save"}
          </button>
        </div>
      </form>

      <div className="rounded-xl border border-danger/30 p-3">
        <div className="text-[12.5px] font-medium">Delete this project</div>
        <p className="mt-0.5 text-[11.5px] text-subtle">Its databases, buckets, streams and repositories stay; only the project and its canvas go.</p>
        <div className="mt-2.5 flex gap-2">
          <input value={confirm} onChange={(e) => setConfirm(e.target.value)} placeholder={project.name} aria-label="Type the project's name to delete it" className={`${field} h-8 flex-1 font-mono text-[12px]`} />
          <button
            disabled={confirm !== project.name || busy}
            onClick={() => workspace.removeProject(project.id).then(() => navigate({ href: "/dashboard" }), (e) => setError(e instanceof Error ? e.message : String(e)))}
            className="h-8 rounded-lg border border-danger/50 px-3 text-[12px] text-danger transition-colors hover:bg-danger/10 disabled:opacity-40"
          >
            Delete
          </button>
        </div>
      </div>
    </div>
  );
}
