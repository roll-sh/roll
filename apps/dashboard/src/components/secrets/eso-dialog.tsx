import { Check, Copy, Download } from "lucide-react";
import { useState } from "react";
import { ROLL_API } from "@/lib/roll-api";
import type { Environment } from "@/lib/secrets-api";
import { esoManifest, esoName, k8sNameProblem, refreshOk } from "@/lib/secrets-eso";
import { Dialog } from "../agents/ui/dialog";
import { useSecrets } from "./secrets-page";
import { button, ErrorNote, errorText, field, ghost, primary, saveText } from "./ui";

/**
 * Pulling a folder into Kubernetes with External Secrets Operator: makes a token for it and shows
 * the manifests once (the token isn't shown again). The same as `roll secrets kubernetes`.
 */
export function EsoDialog({ open, onClose, env, path }: { open: boolean; onClose: () => void; env: Environment; path: string }) {
  const [n, setN] = useState(0);
  return (
    <Dialog
      open={open}
      onClose={onClose}
      onExited={() => setN((k) => k + 1)}
      title="Pull into Kubernetes"
      lead={`External Secrets Operator reads ${env.name}${path === "/" ? "" : ` ${path}`} into a Secret, from inside the cluster: roll never calls it.`}
      className="w-[min(680px,100%)]"
    >
      <Form key={n} env={env} path={path} onClose={onClose} />
    </Dialog>
  );
}

const defaultServer = () => (/^https?:\/\//.test(ROLL_API) ? ROLL_API : `${location.origin}${ROLL_API}`);

function Form({ env, path, onClose }: { env: Environment; path: string; onClose: () => void }) {
  const { api, project } = useSecrets();
  const [name, setName] = useState(esoName(env.slug));
  const [namespace, setNamespace] = useState("");
  const [refresh, setRefresh] = useState("1h");
  const [server, setServer] = useState(defaultServer);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [yaml, setYaml] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const problem =
    (k8sNameProblem(name) && `Name: ${k8sNameProblem(name)}`) ||
    (namespace && k8sNameProblem(namespace, 63) && `Namespace: ${k8sNameProblem(namespace, 63)}`) ||
    (!refreshOk(refresh) && "Refresh: a number of s, m or h (1h)") ||
    (!/^https?:\/\/\S+$/.test(server) && "roll's URL: http(s)://…") ||
    null;

  if (yaml)
    return (
      <div role="status">
        <p className="text-[12.5px] text-muted">
          Apply it with <code className="font-mono text-[11.5px]">kubectl apply -f</code>. It holds a new token that reads only {env.slug}
          {path === "/" ? "" : ` ${path}`}, and isn&apos;t shown again: revoke it from Access → Tokens.
        </p>
        <pre aria-label="Manifests" className="mt-2 max-h-72 overflow-auto rounded-md border border-line-2 bg-panel-2 px-2.5 py-2 font-mono text-[11px] leading-relaxed text-fg">
          {yaml}
        </pre>
        <div className="mt-3 flex flex-wrap items-center justify-end gap-2">
          <button className={button} onClick={() => saveText(`${name}.eso.yaml`, yaml, "application/yaml")}>
            <Download className="size-3.5" /> Download
          </button>
          <button
            className={button}
            onClick={() => {
              void navigator.clipboard?.writeText(yaml);
              setCopied(true);
              setTimeout(() => setCopied(false), 1200);
            }}
          >
            {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy
          </button>
          <button className={primary} onClick={onClose}>
            Done
          </button>
        </div>
      </div>
    );

  const make = async () => {
    setBusy(true);
    setError(null);
    try {
      const t = await api.createToken({ name: `k8s-${name}`, actions: ["read", "reveal"], environments: [env.slug], ...(path === "/" ? {} : { paths: [path] }) });
      setYaml(esoManifest({ server, project: project.id, env: env.slug, path, name, namespace: namespace || undefined, refresh, token: t.token }));
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
        if (!problem) void make();
      }}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="grid gap-1 text-[11.5px] text-muted">
          Secret name
          <input className={`${field} font-mono`} value={name} onChange={(e) => setName(e.target.value.trim())} spellCheck={false} />
        </label>
        <label className="grid gap-1 text-[11.5px] text-muted">
          Namespace <span className="sr-only">(optional)</span>
          <input className={`${field} font-mono`} value={namespace} placeholder="the current one" onChange={(e) => setNamespace(e.target.value.trim())} spellCheck={false} />
        </label>
        <label className="grid gap-1 text-[11.5px] text-muted">
          Refresh every
          <input className={`${field} font-mono`} value={refresh} onChange={(e) => setRefresh(e.target.value.trim())} spellCheck={false} />
        </label>
        <label className="grid gap-1 text-[11.5px] text-muted">
          roll&apos;s URL, as the cluster reaches it
          <input className={`${field} font-mono`} value={server} onChange={(e) => setServer(e.target.value.trim())} spellCheck={false} />
        </label>
      </div>
      <p className="mt-3 text-[11.5px] text-subtle">
        Each refresh is one audited read. Dynamic keys aren&apos;t issued to it (it can&apos;t renew a lease): link them as managed or live. Needs External Secrets
        Operator in the cluster.
      </p>
      <ErrorNote className="mt-2" error={error ?? problem} />
      <div className="mt-4 flex justify-end gap-2">
        <button type="button" className={ghost} onClick={onClose}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={busy || !!problem}>
          Create token and show manifests
        </button>
      </div>
    </form>
  );
}
