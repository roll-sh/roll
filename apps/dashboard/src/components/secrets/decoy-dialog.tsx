import { useState } from "react";
import { SECRET_KEY, type Environment, type Secret } from "@/lib/secrets-api";
import { Dialog } from "../agents/ui/dialog";
import { useSecrets } from "./secrets-page";
import { ErrorNote, errorText, field, ghost, label, primary } from "./ui";

/** Names a found credential would plausibly have. */
const SUGGEST = ["LEGACY_DEPLOY_TOKEN", "ROLL_ADMIN_TOKEN", "BACKUP_API_TOKEN"];

/**
 * Plant a decoy key (docs/secrets.md, "Decoys"): a new key whose value is a roll token that allows
 * nothing. It reads like any other key; any use of it raises `decoy_used` here, in Health and the
 * activity, and at the environment's notify URL. A project admin's.
 */
export function DecoyDialog({ open, onClose, env, path, taken, onPlanted }: { open: boolean; onClose: () => void; env: Environment; path: string; taken: Set<string>; onPlanted: (s: Secret) => void }) {
  const { api } = useSecrets();
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const name = key.trim();
  const problem = !name ? null : !SECRET_KEY.test(name) ? "Letters, digits and _, not starting with a digit" : taken.has(name) ? `${name} is already in this folder: a decoy is a new key` : null;
  const close = () => {
    setKey("");
    setError(null);
    onClose();
  };
  const plant = async () => {
    setBusy(true);
    setError(null);
    try {
      const s = await api.addDecoy(env.slug, { key: name, kind: "rlt", path: path === "/" ? undefined : path });
      onPlanted(s);
      close();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onClose={close}
      busy={busy}
      title="Add a decoy"
      lead={`A key in ${env.name}${path === "/" ? "" : ` ${path}`} whose value is a roll token that opens nothing. Plant it where only an intruder would look; any use of it raises an alert here.`}
      footer={
        <>
          <button className={ghost} onClick={close} disabled={busy}>
            Cancel
          </button>
          <button className={primary} onClick={plant} disabled={busy || !name || problem !== null}>
            {busy ? "Planting…" : "Plant decoy"}
          </button>
        </>
      }
    >
      <div className="space-y-3 text-[12.5px]">
        <label className="block">
          <span className={label}>Key</span>
          <input
            className={`${field} font-mono`}
            value={key}
            onChange={(e) => setKey(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && name && !problem && !busy && void plant()}
            placeholder="LEGACY_DEPLOY_TOKEN"
            spellCheck={false}
            autoFocus
            aria-invalid={problem !== null}
          />
        </label>
        {problem && <p className="text-[11.5px] text-danger">{problem}</p>}
        <div className="flex flex-wrap gap-1.5">
          {SUGGEST.filter((s) => !taken.has(s)).map((s) => (
            <button key={s} className="rounded-md border border-line-2 px-1.5 py-0.5 font-mono text-[11px] text-muted transition-colors hover:border-accent/50 hover:text-fg" onClick={() => setKey(s)}>
              {s}
            </button>
          ))}
        </div>
        <ul className="space-y-1 rounded-lg border border-line-2 bg-panel px-3 py-2.5 text-[12px] text-muted">
          <li>It reads like any other key: only this project's admins see it's a decoy.</li>
          <li>Sync it into CI, export it to an old config: that's the point.</li>
          <li>Changing or deleting the key ends the decoy and revokes its token.</li>
        </ul>
        <ErrorNote error={error} onDismiss={() => setError(null)} />
      </div>
    </Dialog>
  );
}
