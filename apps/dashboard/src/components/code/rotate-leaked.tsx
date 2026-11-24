import { RotateCw } from "lucide-react";
import { useState } from "react";
import { secretsApi } from "@/lib/secrets-api";
import { button, ErrorNote, errorText } from "../secrets/ui";

/**
 * Rotate for a `secret_leak` notice (docs/secrets.md, "When a value leaks"): roll secrets' rotate
 * route, called as the person looking, so their own rights decide. Only where the blocker says
 * roll can rotate the key and the project doesn't rotate leaked keys on its own.
 */
export function RotateLeaked({ project, k, at }: { project: string; k: string; at: { environment: string; folder: string } }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const where = `${at.environment}${at.folder === "/" ? "" : `:${at.folder}`}`;
  const rotate = async () => {
    setBusy(true);
    setError(null);
    try {
      const out = await secretsApi(project).rotate(at.environment, k, at.folder);
      setDone(out.request ? `waiting for approval in ${out.request.id}` : out.change ? `rotated in #${out.change.seq}` : "rotated");
    } catch (e) {
      setError(errorText(e));
    }
    setBusy(false);
  };
  if (done)
    return (
      <span className="text-[12px] text-muted">
        {k} ({where}) {done}: remove the old value from the change
      </span>
    );
  return (
    <span className="inline-flex flex-col gap-1">
      <button className={button} onClick={rotate} disabled={busy} title={`Issue a new ${k} in ${where} and revoke the leaked one`}>
        <RotateCw className={`size-3.5 ${busy ? "animate-spin text-accent" : ""}`} /> Rotate {k} ({where})
      </button>
      <ErrorNote error={error} onDismiss={() => setError(null)} />
    </span>
  );
}
