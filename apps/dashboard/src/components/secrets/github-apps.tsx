import { GitBranch, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import type { GitHubApp } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

const ID = /^[1-9][0-9]{0,15}$/;
const GITHUB = "https://api.github.com";

/**
 * GitHub App tokens (docs/secrets.md, "GitHub App tokens"): the project's own App, whose private
 * key roll keeps sealed; a dynamic link to it gives each lease an installation token narrowed to
 * the link's repositories and permissions, revoked when the lease ends.
 */
export function GitHubApps() {
  const { api } = useSecrets();
  const list = useLoad(() => api.githubApps(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [appId, setAppId] = useState("");
  const [installation, setInstallation] = useState("");
  const [key, setKey] = useState("");
  const [apiBase, setApiBase] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && ID.test(appId) && ID.test(installation) && key.trim().startsWith("-----BEGIN") && (apiBase.trim() === "" || /^https:\/\/\S+$/.test(apiBase.trim()));

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addGitHubApp({ name, app_id: Number(appId), installation_id: Number(installation), private_key: key.trim(), api_base: apiBase.trim() || undefined });
      setAdding(false);
      setName("");
      setAppId("");
      setInstallation("");
      setKey("");
      setApiBase("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (a: GitHubApp) =>
    api.deleteGitHubApp(a.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <Section
      title="GitHub Apps"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add an App
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Your own GitHub App&apos;s installation. Link <span className="font-mono">GITHUB_TOKEN</span> to it, dynamic: each lease gets an installation token that reaches only the repositories and permissions you pick, and GitHub revokes it
        when the lease ends. No personal access token to keep or rotate.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {adding && (
        <form
          className="rise mb-3 grid gap-2 rounded-xl border border-line-2 bg-panel p-3 sm:grid-cols-[11rem_1fr]"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label>
            <span className="mb-1 block text-[11px] font-medium text-subtle">Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="deploy" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <div className="flex flex-wrap gap-2">
            <label className="w-36">
              <span className="mb-1 block text-[11px] font-medium text-subtle">App ID</span>
              <input value={appId} onChange={(e) => setAppId(e.target.value)} inputMode="numeric" placeholder="123456" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
            <label className="w-36">
              <span className="mb-1 block text-[11px] font-medium text-subtle">Installation ID</span>
              <input value={installation} onChange={(e) => setInstallation(e.target.value)} inputMode="numeric" placeholder="7890123" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
          </div>
          <label className="sm:col-start-2">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Private key (kept sealed, never shown again)</span>
            <textarea value={key} onChange={(e) => setKey(e.target.value)} rows={4} placeholder="-----BEGIN RSA PRIVATE KEY-----" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[11px]`} />
          </label>
          <details className="sm:col-start-2">
            <summary className="cursor-pointer text-[11px] text-subtle select-none">GitHub Enterprise Server</summary>
            <input value={apiBase} onChange={(e) => setApiBase(e.target.value)} placeholder="https://github.example.com/api/v3" spellCheck={false} className={`${field} mt-1 font-mono text-[12px]`} aria-label="GitHub API" />
          </details>
          <div className="sm:col-start-2 flex gap-2">
            <button type="submit" className={primary} disabled={!ok || busy}>
              {busy ? "Asking GitHub…" : "Add App"}
            </button>
            <button type="button" className={ghost} onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No GitHub Apps. Add one to give jobs a short-lived token instead of a personal one.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((a) => (
              <div key={a.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <GitBranch className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{a.name}</span>
                <span className="font-mono text-[11.5px] text-subtle">
                  app {a.app_id} · installation {a.installation_id}
                </span>
                {a.api_base !== GITHUB && <span className="font-mono text-[11px] text-muted">{a.api_base}</span>}
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(a.created_at)} · <ActorMark who={a.created_by} />
                </span>
                {deleting === a.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(a)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(a.id)} aria-label={`Delete the GitHub App ${a.name}`}>
                    <Trash2 className="size-3.5" />
                  </button>
                )}
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}
