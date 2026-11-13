import { Search } from "lucide-react";
import { useEffect, useState } from "react";
import { Link } from "@/components/link";
import { findSecret, needsApproval, secretsApi, type Place } from "@/lib/secrets-api";
import { Popover, PopoverPopup, PopoverTrigger } from "../agents/ui/popover";
import { secretsHref } from "./secrets-page";
import { ago, button, errorText, field, primary } from "./ui";

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
      {rows && rows.length > 0 && !key.endsWith("*") && <Everywhere places={rows} />}
    </>
  );
}

type Outcome = { place: Place; result: "set" | "requested" | "rotated" | string };

/**
 * One new value for every place a key is set (rotating an organisation-wide key): one change per
 * environment, a change request where it's protected, a linked place rotated by its provider.
 */
function Everywhere({ places }: { places: Place[] }) {
  const [open, setOpen] = useState(false);
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<Outcome[] | null>(null);
  const run = async () => {
    setBusy(true);
    const out: Outcome[] = [];
    for (const p of places) {
      const api = secretsApi(p.project);
      try {
        if (p.linked) {
          await api.rotate(p.environment, p.key, p.path);
          out.push({ place: p, result: "rotated" });
          continue;
        }
        try {
          await api.set(p.environment, p.key, { value, message: `set ${p.key} everywhere` }, p.path);
          out.push({ place: p, result: "set" });
        } catch (e) {
          if (!needsApproval(e)) throw e;
          await api.openRequest(p.environment, { path: p.path, message: `set ${p.key} everywhere`, ops: [{ op: "set", key: p.key, value }] });
          out.push({ place: p, result: "requested" });
        }
      } catch (e) {
        out.push({ place: p, result: errorText(e) });
      }
    }
    setValue("");
    setDone(out);
    setBusy(false);
  };
  if (done)
    return (
      <div className="mt-2 border-t border-line pt-2 text-[12px]">
        {done.map(({ place: p, result }) => (
          <p key={`${p.project}/${p.environment}/${p.path}`} className="flex gap-2 px-2 py-0.5">
            <span className="min-w-0 flex-1 truncate">
              {p.project_name} · {p.environment}
              {p.path !== "/" && ` ${p.path}`}
            </span>
            <span className={["set", "rotated", "requested"].includes(result) ? "text-muted" : "text-danger"}>{result === "requested" ? "change request opened" : result}</span>
          </p>
        ))}
      </div>
    );
  if (!open)
    return (
      <div className="mt-2 flex items-center justify-end border-t border-line pt-2">
        <button className={button} onClick={() => setOpen(true)} title="One new value in every place above (linked places rotate)">
          Set everywhere…
        </button>
      </div>
    );
  const plain = places.filter((p) => !p.linked).length;
  return (
    <form
      className="mt-2 flex items-center gap-2 border-t border-line pt-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (!busy && (value || plain === 0)) void run();
      }}
    >
      <input autoFocus type="password" value={value} onChange={(e) => setValue(e.target.value)} placeholder={plain ? "the new value" : "linked places rotate"} disabled={!plain} autoComplete="off" className={`${field} font-mono`} aria-label="New value" />
      <button type="submit" className={`${primary} shrink-0`} disabled={busy || (!value && plain > 0)}>
        {busy ? "Setting…" : `Set in ${places.length} ${places.length === 1 ? "place" : "places"}`}
      </button>
    </form>
  );
}
