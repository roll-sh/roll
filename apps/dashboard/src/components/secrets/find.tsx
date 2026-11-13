import { Search } from "lucide-react";
import { useEffect, useState } from "react";
import { Link } from "@/components/link";
import { findSecret, type Place } from "@/lib/secrets-api";
import { Popover, PopoverPopup, PopoverTrigger } from "../agents/ui/popover";
import { secretsHref } from "./secrets-page";
import { ago, errorText, field } from "./ui";

/** Where a key is set across every project the caller may read (`GET /v1/secrets/search`): names and versions, never values. */
export function FindKey() {
  const [open, setOpen] = useState(false);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger className="flex h-7 items-center gap-1.5 rounded-md px-2 text-[12px] text-muted transition-colors hover:bg-white/[0.05] hover:text-fg" title="Find a key in every project">
        <Search className="size-3.5" />
        <span className="max-md:hidden">Find a key</span>
      </PopoverTrigger>
      <PopoverPopup align="end" className="w-[min(30rem,calc(100vw-2rem))] flex-col p-2">
        {open && <Finder onPick={() => setOpen(false)} />}
      </PopoverPopup>
    </Popover>
  );
}

function Finder({ onPick }: { onPick: () => void }) {
  const [q, setQ] = useState("");
  const [rows, setRows] = useState<Place[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const key = q.trim();

  useEffect(() => {
    if (key.length < 2) return setRows(null);
    let live = true;
    const t = setTimeout(() => {
      findSecret(key)
        .then((r) => live && (setRows(r), setError(null)))
        .catch((e) => live && setError(errorText(e)));
    }, 250);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [key]);

  return (
    <>
      <input autoFocus value={q} onChange={(e) => setQ(e.target.value)} placeholder="STRIPE_KEY, or STRIPE_* for a prefix" className={`${field} font-mono`} spellCheck={false} aria-label="Key to find" />
      <div className="mt-2 max-h-80 overflow-y-auto text-[12.5px]">
        {error ? (
          <p className="px-2 py-3 text-danger">{error}</p>
        ) : !rows ? (
          <p className="px-2 py-3 text-subtle">Every project, environment and folder holding a key, among those you can read. Never its value.</p>
        ) : rows.length === 0 ? (
          <p className="px-2 py-3 text-subtle">{key} isn't set anywhere you can read.</p>
        ) : (
          rows.map((p) => (
            <Link
              key={`${p.project}/${p.environment}/${p.path}/${p.key}`}
              href={secretsHref(p.project, { env: p.environment, path: p.path === "/" ? undefined : p.path, key: p.key })}
              onClick={onPick}
              className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 rounded-md px-2 py-1.5 transition-colors hover:bg-fg/[0.05]"
            >
              <span className="min-w-0 truncate">
                <span className="text-fg">{p.project_name}</span>
                <span className="text-subtle"> · {p.environment}</span>
                {p.path !== "/" && <span className="font-mono text-subtle"> {p.path}</span>}
              </span>
              <span className="font-mono text-[11.5px] text-muted tabular-nums">
                {p.key !== key && `${p.key} `}v{p.version} · {ago(p.updated_at)}
                {p.linked && " · linked"}
              </span>
            </Link>
          ))
        )}
        {rows && rows.length >= 500 && <p className="px-2 py-2 text-[11.5px] text-subtle">The first 500. Narrow the key to see the rest.</p>}
      </div>
    </>
  );
}
