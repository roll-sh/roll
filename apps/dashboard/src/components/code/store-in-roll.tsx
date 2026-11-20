import { Check, Copy } from "lucide-react";
import { useEffect, useState } from "react";
import { Link } from "@/components/link";
import { codeApi, type Blocker, type UnstoredStored, type UnstoredTarget } from "@/lib/code-api";
import { Dialog } from "../agents/ui/dialog";
import { button, ErrorNote, errorText, field, ghost, primary } from "../secrets/ui";

/**
 * "Store in roll" for a `secret_unstored` notice (docs/secrets.md, "Credentials roll doesn't hold"):
 * roll reads the credential from the change itself, on the server, and stores it as a new key of the
 * project roll secrets trusts the repo for. The value never reaches the browser. Afterwards it shows
 * the read to write instead: roll never rewrites the change, its author does.
 */
export function StoreInRoll({ change, notice, onStored }: { change: string; notice: Blocker & { fingerprint: string }; onStored: () => void }) {
  const [open, setOpen] = useState(false);
  const [n, setN] = useState(0);
  return (
    <>
      <button className={button} onClick={() => setOpen(true)}>
        Store in roll…
      </button>
      <Dialog
        open={open}
        onClose={() => setOpen(false)}
        onExited={() => setN((k) => k + 1)}
        title="Store in roll"
        lead={`roll reads the credential at ${notice.path}:${notice.line} from the change and keeps it as a new key. Its value is never shown.`}
      >
        <Form key={n} change={change} fingerprint={notice.fingerprint} onClose={() => setOpen(false)} onStored={onStored} />
      </Dialog>
    </>
  );
}

function Form({ change, fingerprint, onClose, onStored }: { change: string; fingerprint: string; onClose: () => void; onStored: () => void }) {
  const [target, setTarget] = useState<UnstoredTarget | null>(null);
  const [key, setKey] = useState("");
  const [env, setEnv] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [stored, setStored] = useState<UnstoredStored | null>(null);

  useEffect(() => {
    let alive = true;
    codeApi.unstoredTarget(change, fingerprint).then(
      (t) => {
        if (!alive) return;
        setTarget(t);
        setKey(t.key);
        setEnv(t.environment ?? t.environments[0] ?? "");
      },
      (e) => alive && setError(errorText(e)),
    );
    return () => {
      alive = false;
    };
  }, [change, fingerprint]);

  if (stored) return <Stored stored={stored} onClose={onClose} />;
  if (!target)
    return (
      <div>
        {error ? <ErrorNote error={error} /> : <p className="shimmer text-[12.5px] text-muted">Finding the project this repo is trusted for…</p>}
        <div className="mt-4 flex justify-end">
          <button type="button" className={ghost} onClick={onClose}>
            {error ? "Close" : "Cancel"}
          </button>
        </div>
      </div>
    );

  const problem = !/^[A-Za-z_][A-Za-z0-9_]*$/.test(key) ? "Key: letters, digits and _, not starting with a digit" : !env ? "Pick an environment" : null;
  const store = async () => {
    setBusy(true);
    setError(null);
    try {
      setStored(await codeApi.storeUnstored(change, fingerprint, { key, environment: env, path: target.path }));
      onStored();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        if (!problem && !busy) void store();
      }}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="grid gap-1 text-[11.5px] text-muted">
          Key
          <input className={`${field} font-mono`} value={key} onChange={(e) => setKey(e.target.value.trim())} spellCheck={false} autoFocus />
        </label>
        <label className="grid gap-1 text-[11.5px] text-muted">
          Environment
          <select value={env} onChange={(e) => setEnv(e.target.value)} className={field}>
            {!env && <option value="">Pick one</option>}
            {target.environments.map((e) => (
              <option key={e} value={e}>
                {e}
              </option>
            ))}
          </select>
        </label>
      </div>
      <p className="mt-3 text-[11.5px] text-subtle">
        In <span className="text-muted">{target.project.name}</span>
        {target.path === "/" ? "" : <> at <span className="font-mono">{target.path}</span></>}, as one change made by you, or a change request where the folder needs
        approval. A key that&apos;s already there isn&apos;t replaced.
      </p>
      <ErrorNote className="mt-2" error={error ?? problem} />
      <div className="mt-4 flex justify-end gap-2">
        <button type="button" className={ghost} onClick={onClose}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={busy || !!problem}>
          {busy ? <span className="shimmer">Storing…</span> : "Store"}
        </button>
      </div>
    </form>
  );
}

function Stored({ stored, onClose }: { stored: UnstoredStored; onClose: () => void }) {
  const [copied, setCopied] = useState(false);
  const where = `${stored.environment}${stored.path === "/" ? "" : ` ${stored.path}`}`;
  return (
    <div role="status">
      <p className="text-[12.5px] text-muted">
        {stored.request ? (
          <>
            Opened a change request to add <span className="font-mono text-fg">{stored.key}</span> to {where}: it&apos;s stored once it&apos;s approved.
          </>
        ) : (
          <>
            Stored as <span className="font-mono text-fg">{stored.key}</span> in {where}.
          </>
        )}
      </p>
      <p className="mt-3 text-[11.5px] text-subtle">
        Replace the value at {stored.file}:{stored.line} with
      </p>
      <div className="mt-1 flex items-center gap-2 rounded-md border border-line-2 bg-panel-2 py-1.5 pr-1.5 pl-2.5">
        <code aria-label="Replacement" className="min-w-0 flex-1 truncate font-mono text-[12px] text-fg">
          {stored.replacement}
        </code>
        <button
          type="button"
          className={button}
          onClick={() => {
            void navigator.clipboard?.writeText(stored.replacement);
            setCopied(true);
            setTimeout(() => setCopied(false), 1200);
          }}
        >
          {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy
        </button>
      </div>
      <p className="mt-3 text-[11.5px] text-subtle">
        roll never rewrites the change: its author still has to change the code, and rotate the key if it was ever pushed anywhere else. Until then the change adds a
        value roll secrets holds.
      </p>
      <div className="mt-4 flex flex-wrap items-center justify-end gap-2">
        <Link
          href={`/dashboard/projects/${stored.project}/secrets?env=${encodeURIComponent(stored.environment)}&key=${encodeURIComponent(stored.key)}`}
          className="mr-auto text-[12px] text-accent hover:underline"
        >
          Open in roll secrets
        </Link>
        <button className={primary} onClick={onClose}>
          Done
        </button>
      </div>
    </div>
  );
}
