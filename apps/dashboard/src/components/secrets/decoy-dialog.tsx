import { Database, KeyRound } from "lucide-react";
import { useState } from "react";
import { SECRET_KEY, type Environment, type LinkTarget, type NewDecoy, type Secret } from "@/lib/secrets-api";
import { Dialog } from "../agents/ui/dialog";
import { Segmented } from "../agents/ui/toggle-group";
import { useSecrets } from "./secrets-page";
import { ErrorNote, errorText, field, ghost, label, primary, useLoad } from "./ui";

type Kind = NewDecoy["kind"];

/** What each kind of decoy is: its value, names a found one would plausibly have, and how it ends. */
const KINDS: Record<Kind, { value: string; suggest: string[]; ends: string }> = {
  rlt: {
    value: "a roll token that opens nothing",
    suggest: ["LEGACY_DEPLOY_TOKEN", "ROLL_ADMIN_TOKEN", "BACKUP_API_TOKEN"],
    ends: "Changing or deleting the key ends the decoy and revokes its token.",
  },
  postgres: {
    value: "a Postgres login that always fails, never wakes the database, and alerts on any use",
    suggest: ["LEGACY_DATABASE_URL", "REPORTING_DB_URL", "PG_ADMIN_URL"],
    ends: "Changing or deleting the key ends the decoy and drops its role.",
  },
};

/**
 * Plant a decoy key (docs/secrets.md, "Decoys"): a new key whose value is a decoy credential, a
 * roll token that allows nothing (`rlt`) or a login to one of the project's Postgres databases
 * that never logs in (`postgres`). It reads like any other key; any use of it raises `decoy_used`
 * here, in Health and the activity, and at the environment's notify URL. A project admin's.
 */
export function DecoyDialog({ open, onClose, env, path, taken, onPlanted }: { open: boolean; onClose: () => void; env: Environment; path: string; taken: Set<string>; onPlanted: (s: Secret) => void }) {
  const { api } = useSecrets();
  const [kind, setKind] = useState<Kind>("rlt");
  const [picked, setPicked] = useState<string | null>(null);
  // The project's Postgres databases, as the links dialog lists them (`GET …/links`): what the API lets a decoy's role be made in.
  const links = useLoad(() => (kind === "postgres" ? api.links() : Promise.resolve([])), [api, kind]);
  const dbs = (links.data ?? []).filter((t) => t.kind === "postgres");
  const db = (dbs.find((d) => d.ref === picked) ?? dbs[0])?.ref ?? null;
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const name = key.trim();
  const problem = !name ? null : !SECRET_KEY.test(name) ? "Letters, digits and _, not starting with a digit" : taken.has(name) ? `${name} is already in this folder: a decoy is a new key` : null;
  const ready = !!name && problem === null && (kind === "rlt" || db !== null);
  const close = () => {
    setKind("rlt");
    setPicked(null);
    setKey("");
    setError(null);
    onClose();
  };
  const plant = async () => {
    setBusy(true);
    setError(null);
    const at = path === "/" ? undefined : path;
    const d: NewDecoy = kind === "postgres" && db ? { key: name, kind, resource: db, path: at } : { key: name, kind: "rlt", path: at };
    try {
      const s = await api.addDecoy(env.slug, d);
      onPlanted(s);
      close();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const about = KINDS[kind];
  return (
    <Dialog
      open={open}
      onClose={close}
      busy={busy}
      title="Add a decoy"
      lead={`A key in ${env.name}${path === "/" ? "" : ` ${path}`} whose value is ${about.value}. Plant it where only an intruder would look; any use of it raises an alert here.`}
      footer={
        <>
          <button className={ghost} onClick={close} disabled={busy}>
            Cancel
          </button>
          <button className={primary} onClick={plant} disabled={busy || !ready}>
            {busy ? "Planting…" : "Plant decoy"}
          </button>
        </>
      }
    >
      <div className="space-y-3 text-[12.5px]">
        <div>
          <span className={label}>Kind</span>
          <Segmented
            label="Kind of decoy"
            value={kind}
            onChange={(k) => (setKind(k), setError(null))}
            options={[
              { value: "rlt", label: <><KeyRound className="size-3.5" strokeWidth={1.5} /> Roll token</> },
              { value: "postgres", label: <><Database className="size-3.5" strokeWidth={1.5} /> Postgres login</> },
            ]}
          />
        </div>
        {kind === "postgres" && <Databases dbs={links.data && dbs} error={links.error} value={db} onChange={setPicked} />}
        <label className="block">
          <span className={label}>Key</span>
          <input
            className={`${field} font-mono`}
            value={key}
            onChange={(e) => setKey(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && ready && !busy && void plant()}
            placeholder={about.suggest[0]}
            spellCheck={false}
            autoFocus
            aria-invalid={problem !== null}
          />
        </label>
        {problem && <p className="text-[11.5px] text-danger">{problem}</p>}
        <div className="flex flex-wrap gap-1.5">
          {about.suggest.filter((s) => !taken.has(s)).map((s) => (
            <button key={s} className="rounded-md border border-line-2 px-1.5 py-0.5 font-mono text-[11px] text-muted transition-colors hover:border-accent/50 hover:text-fg" onClick={() => setKey(s)}>
              {s}
            </button>
          ))}
        </div>
        <ul className="space-y-1 rounded-lg border border-line-2 bg-panel px-3 py-2.5 text-[12px] text-muted">
          <li>It reads like any other key: only this project's admins see it's a decoy.</li>
          <li>Sync it into CI, export it to an old config: that's the point.</li>
          <li>{about.ends}</li>
        </ul>
        <ErrorNote error={error} onDismiss={() => setError(null)} />
      </div>
    </Dialog>
  );
}

/** Which database a Postgres decoy's role is made in: `dbs` null while they load. */
function Databases({ dbs, error, value, onChange }: { dbs: LinkTarget[] | null; error: string | null; value: string | null; onChange: (ref: string) => void }) {
  const { project } = useSecrets();
  if (error) return <ErrorNote error={error} />;
  if (!dbs) return <p className="shimmer text-[12px]">Loading the project's databases…</p>;
  if (!dbs.length) return <p className="rounded-lg border border-line-2 px-3 py-2.5 text-[12px] text-muted">{project.name} has no Postgres database on its canvas. Add one there, then plant a Postgres decoy in it.</p>;
  return (
    <label className="block">
      <span className={label}>Database</span>
      <select value={value ?? ""} onChange={(e) => onChange(e.target.value)} className={`${field} text-[12px]`}>
        {dbs.map((d) => (
          <option key={d.ref} value={d.ref}>
            {d.name}
          </option>
        ))}
      </select>
    </label>
  );
}
