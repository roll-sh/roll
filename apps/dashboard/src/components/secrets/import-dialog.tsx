import { useMemo, useState } from "react";
import { needsApproval, type Environment, type Secret } from "@/lib/secrets-api";
import { parseDotenv, previewImport } from "@/lib/secrets-dotenv";
import { Dialog } from "../agents/ui/dialog";
import { Switch } from "../agents/ui/switch";
import { useSecrets } from "./secrets-page";
import { ErrorNote, errorText, field, ghost, primary } from "./ui";

const ACTION_TONE = { new: "text-ok", replace: "text-accent", skip: "text-subtle" } as const;

/**
 * Bulk paste: a `.env` file, previewed here (which keys are new, replaced or skipped against the
 * folder), then imported by the server as one change. The text is dropped when the dialog closes.
 */
export function ImportDialog({
  open,
  onClose,
  env,
  path,
  current,
  onImported,
}: {
  open: boolean;
  onClose: () => void;
  env: Environment;
  path: string;
  current: Secret[];
  onImported: (summary: string, warnings: string[]) => void;
}) {
  const [n, setN] = useState(0);
  return (
    <Dialog
      open={open}
      onClose={onClose}
      onExited={() => setN((k) => k + 1)}
      title={`Paste a .env into ${env.name}`}
      lead={`Into ${path}. Each line is KEY=value; comments above a key become its comment.`}
      className="w-[min(640px,100%)]"
    >
      <Form key={n} env={env} path={path} current={current} onClose={onClose} onImported={onImported} />
    </Dialog>
  );
}

function Form({ env, path, current, onClose, onImported }: { env: Environment; path: string; current: Secret[]; onClose: () => void; onImported: (summary: string, warnings: string[]) => void }) {
  const { api } = useSecrets();
  const [text, setText] = useState("");
  const [overwrite, setOverwrite] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [needsRequest, setNeedsRequest] = useState(false);
  const parsed = useMemo(() => parseDotenv(text), [text]);
  const rows = useMemo(() => previewImport(parsed.entries, current, overwrite), [parsed.entries, current, overwrite]);
  const writes = rows.filter((r) => r.action !== "skip").length;

  const run = async (asRequest: boolean) => {
    setBusy(true);
    setError(null);
    const body = { dotenv: text, path, overwrite, message: `imported ${writes} ${writes === 1 ? "key" : "keys"} from a .env` };
    try {
      if (asRequest) {
        const r = await api.importAsRequest(env.slug, body);
        onImported(`Opened change request ${r.request.id} for ${r.request.items.length} keys${r.skipped.length ? `, skipped ${r.skipped.length}` : ""}`, []);
      } else {
        const r = await api.importDotenv(env.slug, body);
        onImported(`Imported ${r.items.length} ${r.items.length === 1 ? "key" : "keys"}${r.change ? ` as change #${r.change.seq}` : ""}${r.skipped.length ? `, skipped ${r.skipped.length} that exist` : ""}`, r.warnings);
      }
      setText("");
      onClose();
    } catch (e) {
      if (needsApproval(e)) setNeedsRequest(true);
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-3">
      <textarea
        autoFocus
        value={text}
        onChange={(e) => setText(e.target.value)}
        rows={8}
        spellCheck={false}
        autoComplete="off"
        placeholder={"# the database\nDATABASE_URL=postgres://…\nSTRIPE_KEY=sk_live_…"}
        aria-label=".env contents"
        className={`${field} h-auto resize-y py-2 font-mono text-[12px] leading-relaxed`}
      />
      {parsed.error && <p className="text-[11.5px] text-danger">{parsed.error}</p>}
      {rows.length > 0 && (
        <div className="max-h-56 overflow-y-auto rounded-lg border border-line-2 bg-panel px-2.5 py-1 inset-shadow-highlight">
          {rows.map((r) => (
            <div key={r.key} className="flex items-center gap-2 py-1 text-[12px]">
              <span className={`w-14 shrink-0 text-[11px] font-medium ${ACTION_TONE[r.action]}`}>{r.action === "replace" ? "replace" : r.action}</span>
              <span className="min-w-0 flex-1 truncate font-mono">{r.key}</span>
              {r.over && <span className="truncate text-[11px] text-subtle">overrides the key {r.over}</span>}
              <span className="font-mono text-[10.5px] text-faint">line {r.line}</span>
            </div>
          ))}
        </div>
      )}
      <label className="flex items-center gap-2 text-[12px] text-muted">
        <Switch size="sm" checked={overwrite} onCheckedChange={setOverwrite} />
        Replace keys that exist (otherwise they're skipped)
      </label>
      <ErrorNote error={error} />
      <div className="flex items-center justify-end gap-2 border-t border-line pt-3">
        <span className="mr-auto text-[11.5px] text-subtle">
          {rows.length > 0 && `${rows.filter((r) => r.action === "new").length} new · ${rows.filter((r) => r.action === "replace").length} replaced · ${rows.filter((r) => r.action === "skip").length} skipped`}
        </span>
        <button className={ghost} onClick={onClose}>
          Cancel
        </button>
        {needsRequest ? (
          <button className={primary} disabled={busy || !writes} onClick={() => run(true)}>
            Open a change request
          </button>
        ) : (
          <button className={primary} disabled={busy || !writes || !!parsed.error} onClick={() => run(false)}>
            {busy ? "Importing…" : `Import ${writes || ""}`.trim()}
          </button>
        )}
      </div>
    </div>
  );
}
