import { Lock, LockOpen } from "lucide-react";
import { useState } from "react";
import type { Environment } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { errorText, useLoad } from "./ui";

/**
 * A folder's change policy (docs/secrets.md, "Change policies per folder"): whether changes here
 * need an approved request, and from which folder's policy; an admin turns it on or off for the
 * folder shown. Nothing for a protected environment, where every change needs one already.
 */
export function FolderPolicyChip({ env, path }: { env: Environment; path: string }) {
  const { api } = useSecrets();
  const list = useLoad(() => api.folderPolicies(env.slug), [api, env.slug]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (env.protected || path === "/" || !list.data) return null;
  const covering = list.data.filter((p) => path === p.path || path.startsWith(`${p.path}/`)).sort((a, b) => b.path.length - a.path.length)[0];
  const own = covering?.path === path;
  const toggle = async () => {
    setBusy(true);
    setError(null);
    try {
      if (own) await api.deleteFolderPolicy(env.slug, path);
      else await api.setFolderPolicy(env.slug, path);
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const title = covering
    ? `Changes here need ${covering.approvals} approval${covering.approvals === 1 ? "" : "s"}${covering.approvers.length ? ` from ${covering.approvers.join(", ")}` : ""} (the policy on ${covering.path})${own ? ". Click to remove it (admin)." : ""}`
    : "Require approved change requests for this folder and the folders under it (admin)";
  return (
    <span className="inline-flex items-center gap-1">
      <button
        onClick={() => (own || !covering ? void toggle() : undefined)}
        disabled={busy || (!!covering && !own)}
        title={error ?? title}
        aria-label={covering ? `Changes to ${path} need approval` : `Require approval for ${path}`}
        className={`inline-flex h-6 items-center gap-1 rounded-md px-1.5 text-[11.5px] transition-colors hover:bg-fg/[0.05] ${covering ? "text-fg" : "text-muted hover:text-fg"} ${error ? "text-danger" : ""}`}
      >
        {covering ? <Lock className="size-3" /> : <LockOpen className="size-3" />}
        {covering ? "Needs approval" : "Require approval"}
      </button>
    </span>
  );
}
