import { ChevronRight } from "lucide-react";
import { Fragment } from "react";
import { crumbs } from "@/lib/secrets-grid";

/** The folder breadcrumb: `/ › api › stripe`, each a button to that folder. */
export function FolderCrumbs({ path, onPath }: { path: string; onPath: (p: string) => void }) {
  const parts = crumbs(path);
  return (
    <span className="flex min-w-0 items-center gap-0.5 font-mono text-[12px]" aria-label="Folder">
      {parts.map((c, i) => (
        <Fragment key={c.path}>
          {i > 1 && <ChevronRight className="size-3 shrink-0 text-faint" />}
          <button
            onClick={() => onPath(c.path)}
            disabled={i === parts.length - 1}
            className={`rounded px-1 py-0.5 transition-colors ${i === parts.length - 1 ? "text-fg" : "text-muted hover:bg-fg/[0.05] hover:text-fg"}`}
          >
            {c.name}
          </button>
        </Fragment>
      ))}
    </span>
  );
}
