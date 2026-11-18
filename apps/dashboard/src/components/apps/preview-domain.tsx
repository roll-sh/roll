import { Globe, RefreshCw, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, type Domain } from "@/lib/apps-api";
import { btn, input, Panel, primary, useCopy } from "../ui";
import { Record } from "./domains";
import { SureButton, type Runner } from "./shared";

/** A project's own preview domain (Vercel's preview deployment suffix): a wildcard its preview
 *  environments answer under (`<service>-<change>.preview.acme.dev`), verified as a custom domain
 *  is. Plain HTTP until wildcard certificates are issued here. */
export function PreviewDomainPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [d, setD] = useState<Domain | null | undefined>(undefined);
  const [host, setHost] = useState("");
  const { copied, copy } = useCopy();
  const load = () => appsApi.previewDomain(project).then(setD, () => setD(null));
  useEffect(() => {
    void load();
  }, [project]);
  const busy = runner.busy !== null;
  if (d === undefined) return null;
  return (
    <Panel title="Preview domain">
      <div className="space-y-3 px-5 py-4 text-[13px]">
        <p className="text-muted">
          {projectName}'s previews answer under a domain of its own (<span className="font-mono">web-pr12.preview.acme.dev</span>), so cookies on its domain and OAuth redirect allowlists work. Plain
          HTTP for now: wildcard certificates aren't issued here yet.
        </p>
        {d === null ? (
          <form
            className="flex flex-wrap gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              void runner.run("preview-domain", () => appsApi.setPreviewDomain(project, host.trim())).then((x) => x && (setD(x), setHost("")));
            }}
          >
            <input value={host} onChange={(e) => setHost(e.target.value)} placeholder="*.preview.acme.dev" className={`${input} min-w-64 flex-1 font-mono text-[12px]`} aria-label="Preview domain" />
            <button type="submit" className={primary} disabled={busy || !host.trim().startsWith("*.")}>
              <Globe className="size-3.5" /> Add
            </button>
          </form>
        ) : (
          <div className="space-y-2">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <span className="font-mono text-[12.5px] text-fg">{d.hostname}</span>
              <span className="flex items-center gap-2">
                <span className={`text-[12px] ${d.status === "active" ? "text-fg" : "text-muted"}`}>{d.status === "active" ? "verified" : "waiting for its DNS"}</span>
                {d.status !== "active" && (
                  <button type="button" className={btn} disabled={busy} onClick={() => void runner.run("preview-verify", () => appsApi.verifyPreviewDomain(project)).then((x) => x && setD(x))}>
                    <RefreshCw className="size-3.5" /> Verify
                  </button>
                )}
                <SureButton className={btn} disabled={busy} sure="Sure? Its previews stop answering there" onSure={() => runner.run("preview-remove", () => appsApi.removePreviewDomain(project)).then(() => setD(null))}>
                  <Trash2 className="size-3.5" /> Remove
                </SureButton>
              </span>
            </div>
            {d.status !== "active" && (
              <div className="space-y-1.5">
                <table className="w-full overflow-hidden rounded-lg border border-line font-mono text-[12px]">
                  <tbody>
                    {d.record !== "txt" && <Record type="CNAME" name={d.hostname} value={d.target} copied={copied} copy={copy} />}
                    {d.token && <Record type="TXT" name={`_roll-challenge.${d.hostname.replace(/^\*\./, "")}`} value={d.token} copied={copied} copy={copy} />}
                  </tbody>
                </table>
                {d.error && <p className="text-[12px] text-danger">{d.error}</p>}
              </div>
            )}
          </div>
        )}
      </div>
    </Panel>
  );
}
