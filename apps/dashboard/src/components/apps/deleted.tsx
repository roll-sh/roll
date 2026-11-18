import { RotateCcw, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, type DeletedService } from "@/lib/apps-api";
import { loadWorkspace } from "@/lib/workspace-store";
import { btn, Confirm, danger, Panel } from "../ui";
import { since, until, type Runner } from "./shared";

/** A project's deleted services, kept stopped for 48 hours (Railway's): each restored as it was,
 *  or deleted for good now (which frees its name). Shown only while there are some. */
export function DeletedPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [deleted, setDeleted] = useState<DeletedService[] | null>(null);
  const [purging, setPurging] = useState<string | null>(null);
  const load = () =>
    appsApi
      .deletedServices(project)
      .then(setDeleted)
      .catch(() => setDeleted([]));
  useEffect(() => {
    void load();
  }, [project]);
  if (!deleted?.length) return null;
  const restore = (s: DeletedService) =>
    runner.run(`restore:${s.id}`, async () => {
      await appsApi.restoreService(s.id);
      // Back on the canvas where the server placed it.
      await Promise.all([load(), loadWorkspace()]);
    });
  const purge = (s: DeletedService) =>
    runner.run(`purge:${s.id}`, async () => {
      await appsApi.remove(s.id);
      setPurging(null);
      await load();
    });
  return (
    <Panel title="Recently deleted">
      <p className="px-5 pt-4 text-[13px] text-muted">
        {projectName}'s deleted services, stopped. Each is deleted for good 48 hours after it was deleted, with its deployments, images and volume.
      </p>
      <ul className="divide-y divide-line px-5 py-2">
        {deleted.map((s) => (
          <li key={s.id} className="py-3">
            {purging === s.id ? (
              <Confirm word={s.name} action="Delete for good" busy={runner.busy === `purge:${s.id}`} onCancel={() => setPurging(null)} onConfirm={() => void purge(s)}>
                Deletes <span className="font-mono text-fg">{s.name}</span>'s environments, deployments, images and volume now (a last backup is kept for a week), and frees its name.
              </Confirm>
            ) : (
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="truncate font-mono text-[13px] text-fg">{s.name}</div>
                  <div className="text-[12px] text-subtle">
                    deleted {since(s.deleted_at)}, gone for good {until(s.purge_at)}
                  </div>
                </div>
                <div className="flex gap-2">
                  <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void restore(s)}>
                    <RotateCcw className="size-3.5" /> {runner.busy === `restore:${s.id}` ? <span className="shimmer">Restoring…</span> : "Restore"}
                  </button>
                  <button type="button" className={danger} disabled={runner.busy !== null} onClick={() => setPurging(s.id)}>
                    <Trash2 className="size-3.5" /> Delete now
                  </button>
                </div>
              </div>
            )}
          </li>
        ))}
      </ul>
    </Panel>
  );
}
