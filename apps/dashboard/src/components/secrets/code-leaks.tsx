import { FileWarning, ScanSearch } from "lucide-react";
import { useState } from "react";
import { Link } from "@/components/link";
import { codeApi, type LeakScan } from "@/lib/code-api";
import { blobHref } from "@/components/repo/paths";
import { useSecrets } from "./secrets-page";
import { button, card, ErrorNote, errorText, Section } from "./ui";

/**
 * Values in code (docs/secrets.md, "Leaked secrets don't land"): a scan of each canvas repo's
 * default branch for this project's stored values, by fingerprint. Where, never what.
 */
export function CodeLeaks() {
  const { project } = useSecrets();
  const repos = project.resources.filter((r) => r.kind === "repo").map((r) => r.ref);
  if (repos.length === 0) return null;
  return (
    <Section title="Values in code">
      <p className="mb-2 text-[11.5px] text-muted">New changes that add a secret's value can't land. A scan finds values already on a repo's default branch: rotate the secret, then remove it.</p>
      <div className={`${card} divide-y divide-line`}>
        {repos.map((r) => (
          <RepoScan key={r} repo={r} />
        ))}
      </div>
    </Section>
  );
}

function RepoScan({ repo }: { repo: string }) {
  const [scan, setScan] = useState<LeakScan | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      setScan(await codeApi.scanLeaks(repo));
    } catch (e) {
      setError(errorText(e));
    }
    setBusy(false);
  };
  const found = scan?.found ?? [];
  return (
    <div className="px-3 py-2">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span className="font-mono text-[12px]">{repo}</span>
        <span className="text-[11px] text-subtle">
          {busy
            ? "scanning…"
            : !scan
              ? "not scanned"
              : !scan.head
                ? "nothing on the default branch yet"
                : `${scan.files} file${scan.files === 1 ? "" : "s"} · ${found.length ? `${found.length} value${found.length === 1 ? "" : "s"} found` : "no values found"}${scan.truncated ? " · stopped at the limit" : ""}`}
        </span>
        <button className={`${button} ml-auto`} onClick={run} disabled={busy}>
          <ScanSearch className={`size-3.5 ${busy ? "animate-pulse text-accent" : ""}`} /> {scan ? "Scan again" : "Scan"}
        </button>
      </div>
      <ErrorNote error={error} onDismiss={() => setError(null)} className="mt-2" />
      {found.length > 0 && scan?.head && (
        <ul className="rise mt-2 space-y-1">
          {found.map((f) => (
            <li key={`${f.key}:${f.environment}:${f.path}:${f.line}`} className="flex flex-wrap items-center gap-x-2 text-[12px]">
              <FileWarning className="size-3.5 text-danger" />
              <span className="font-mono">{f.key}</span>
              <span className="text-subtle">({f.environment})</span>
              <span className="text-subtle">in</span>
              <Link href={`${blobHref(repo, scan.head as string, f.path)}#L${f.line}`} className="font-mono text-accent hover:underline">
                {f.path}:{f.line}
              </Link>
            </li>
          ))}
        </ul>
      )}
      {scan && scan.skipped.length > 0 && <p className="mt-1 text-[11px] text-subtle">Not read: {scan.skipped.slice(0, 5).join(", ")}{scan.skipped.length > 5 ? ` and ${scan.skipped.length - 5} more` : ""}</p>}
    </div>
  );
}
