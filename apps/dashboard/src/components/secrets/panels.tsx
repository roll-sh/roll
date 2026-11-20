import { useLocation } from "@tanstack/react-router";
import { useEffect, type ReactNode } from "react";
import { PANELS, type Panel } from "@/lib/secrets-tabs";
import { AtlasProjects } from "./atlas-projects";
import { AwsRoles } from "./aws-roles";
import { Cas } from "./cas";
import { CloudAccounts } from "./cloud-accounts";
import { Directories } from "./directories";
import { Gateways } from "./gateways";
import { GitHubApps } from "./github-apps";
import { KubernetesClusters } from "./kubernetes-clusters";
import { OutsideDatabases } from "./outside-databases";
import { Rotators } from "./rotators";
import { SaasRotators } from "./saas-rotators";
import { SnowflakeAccounts } from "./snowflake-accounts";
import { SshCas } from "./ssh-cas";
import { TransitKeys } from "./transit-keys";

const PANEL: Record<Exclude<Panel, "syncs">, () => ReactNode> = {
  rotators: Rotators,
  "saas-rotators": SaasRotators,
  "outside-databases": OutsideDatabases,
  directories: Directories,
  snowflake: SnowflakeAccounts,
  atlas: AtlasProjects,
  gateways: Gateways,
  "cloud-accounts": CloudAccounts,
  "aws-roles": AwsRoles,
  "github-apps": GitHubApps,
  kubernetes: KubernetesClusters,
  "transit-keys": TransitKeys,
  cas: Cas,
  "ssh-cas": SshCas,
};

/** One tab's panels in order, each at its anchor (`#rotators`), scrolled to when the link names one. */
function Panels({ panels }: { panels: readonly Exclude<Panel, "syncs">[] }) {
  const hash = useLocation({ select: (l) => l.hash.replace(/^#/, "") });
  useEffect(() => {
    if (hash) document.getElementById(hash)?.scrollIntoView({ block: "start" });
  }, [hash]);
  return (
    <div className="mx-auto max-w-4xl space-y-8 px-4 py-5">
      {panels.map((p) => {
        const Body = PANEL[p];
        return (
          <div key={p} id={p} className="scroll-mt-4">
            <Body />
          </div>
        );
      })}
    </div>
  );
}

/** The outside systems roll issues credentials for or rotates them in, and the gateways to private ones. */
export function CredentialsView() {
  return <Panels panels={PANELS.credentials} />;
}

/** The keys roll holds for the project: transit encryption, and its TLS and SSH certificate authorities. */
export function KeysView() {
  return <Panels panels={PANELS.keys} />;
}
