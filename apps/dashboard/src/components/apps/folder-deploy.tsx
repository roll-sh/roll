import { FolderUp } from "lucide-react";
import { useRef, useState } from "react";
import { appsApi, SERVICE_NAME, type Deployment, type Service } from "@/lib/apps-api";
import { bundleOf, filesFromDrop } from "@/lib/bundle";

/** A folder's name as a service's (a-z, 0-9 and -). */
export function serviceName(folder: string): string {
  const n = folder.toLowerCase().replace(/[^a-z0-9-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 32).replace(/-+$/, "");
  return SERVICE_NAME.test(n) ? n : "site";
}

/**
 * Drop a folder to deploy it (Netlify Drop): its files bundled in the browser as `roll up`
 * bundles them (`lib/bundle.ts`, `.env` files and `node_modules` left out) and uploaded to this
 * upload service's environment. A folder picker for whoever can't drag. With `make` (New
 * service, an empty project) the upload service is made first, named after the folder.
 */
export function FolderDeploy({
  service,
  env,
  onDeployed,
  make,
  compact = false,
}: {
  service?: string;
  env: string;
  onDeployed: (d: Deployment, s?: Service) => void;
  /** Make the service the folder deploys to (from the folder's name). */
  make?: (name: string) => Promise<Service>;
  compact?: boolean;
}) {
  const [over, setOver] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const picker = useRef<HTMLInputElement>(null);
  const send = async (files: { path: string; file: File }[], name: string) => {
    setError(null);
    try {
      setBusy(`Bundling ${files.length} files…`);
      const b = await bundleOf(files);
      let made: Service | undefined;
      let id = service;
      if (!id) {
        if (!make) throw new Error("no service to deploy to");
        setBusy(`Making ${serviceName(name)}…`);
        made = await make(serviceName(name));
        id = made.id;
      }
      setBusy(`Uploading ${Math.max(1, Math.round(b.body.size / 1024))} KiB…`);
      const d = await appsApi.uploadDeploy(id, env, b.body, `${name}, dropped (${b.files} ${b.files === 1 ? "file" : "files"})`);
      onDeployed(d, made);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <div
      onDragOver={(e) => {
        e.preventDefault();
        setOver(true);
      }}
      onDragLeave={() => setOver(false)}
      onDrop={(e) => {
        e.preventDefault();
        setOver(false);
        void filesFromDrop(e.dataTransfer.items).then(({ name, files }) => send(files, name), (err: unknown) => setError(String(err)));
      }}
      className={`flex flex-col items-center gap-2 rounded-lg border border-dashed px-4 text-center transition-colors ${compact ? "py-3" : "py-6"} ${over ? "border-accent bg-accent/[0.04]" : "border-line-2"}`}
    >
      <FolderUp className="size-4 text-muted" />
      {busy ? (
        <p className="shimmer text-[12.5px] text-muted">{busy}</p>
      ) : (
        <p className="text-[12.5px] text-muted">
          {make ? "Drop your site's or app's folder here to make a service and deploy it, or" : "Drop a folder here to deploy it, or"}{" "}
          <button type="button" className="text-accent hover:underline" onClick={() => picker.current?.click()}>
            choose one
          </button>
          . Its <code className="font-mono">.env</code> files and <code className="font-mono">node_modules</code> stay behind.
        </p>
      )}
      {error && <p className="text-[12px] text-danger">{error}</p>}
      <input
        ref={picker}
        type="file"
        className="hidden"
        aria-label="Choose a folder to deploy"
        // A folder, not files: its paths come with it.
        {...({ webkitdirectory: "", directory: "" } as Record<string, string>)}
        onChange={(e) => {
          const list = Array.from(e.target.files ?? []);
          e.target.value = "";
          if (!list.length) return;
          const root = list[0]!.webkitRelativePath.split("/")[0] ?? "site";
          void send(list.map((f) => ({ path: f.webkitRelativePath.slice(root.length + 1), file: f })), root);
        }}
      />
    </div>
  );
}
