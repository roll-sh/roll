"use client";

import { closeTopLayer } from "./escape-layers";
import { useMemo } from "react";
import type { Machine, Project } from "@/lib/agents-api";
import type { AgentsData } from "@/lib/agents-store";
import { ProjectIcon } from "./identity";
import { machineSettingItems, MachineSettingsPanel } from "./machine-settings";
import { ownerSettingItems, OwnerSettingsPanel } from "./owner-settings-panel";
import { ProjectSettingsForm } from "./project-settings";
import { SettingRow, SettingsPage, SettingsSection, type SettingsElsewhere } from "./settings-ui";
import { shortPath } from "./shared";
import { useAgents, useAgentsSnapshot } from "./use-agents";

/**
 * Settings in the side panel beside the canvas (`{ kind: "settings", target }`,
 * `#agents/settings/<target>`): the owner's (`owner`, from the agents header and ⌘,), a machine's
 * (`m:<id>`, from its card's gear) or a project's (`p:<id>`, from its card's menu). The card stays
 * on the canvas, selected, while its settings are open.
 */
export function SettingsPanel({ target, onClose }: { target: string; onClose: () => void }) {
  const { client } = useAgents();
  // Its own subscription: what the daemon says back (a machine's `hello`) shows at once.
  const data = useAgentsSnapshot(client)?.data ?? client.data;
  const elsewhere = useElsewhere(data, target);
  if (target === "owner") return <OwnerSettingsPanel onClose={onClose} elsewhere={elsewhere} />;
  const id = target.slice(2);
  if (target.startsWith("m:")) {
    const machine = data.machines.get(id)?.value;
    return machine ? <MachineSettingsPanel key={machine.id} machine={machine} onClose={onClose} elsewhere={elsewhere} /> : <Gone onClose={onClose} />;
  }
  if (target.startsWith("p:")) {
    const r = data.projects.get(id);
    if (!r) return <Gone onClose={onClose} />;
    const machine = data.machines.get(r.machine)?.value;
    return (
      // The × asks what's open inside first (unsaved project settings hold it: `escape-layers.ts`).
      <SettingsPage icon={<ProjectIcon project={r.value} className="size-4" />} title={r.value.name} subtitle="Project settings" onClose={() => closeTopLayer() || onClose()} items={[]} search={false}>
        <ProjectOverview project={r.value} machine={machine} />
        <ProjectSettingsForm project={r.value} machine={machine} />
      </SettingsPage>
    );
  }
  return <Gone onClose={onClose} />;
}

/** The settings pages other than `target`, for its search: the owner's, then each machine's. */
function useElsewhere(data: AgentsData, target: string): SettingsElsewhere[] {
  const { services } = useAgents();
  const machines = useMemo(() => [...data.machines.values()].map((r) => r.value).sort((a, b) => a.name.localeCompare(b.name)), [data.machines]);
  return useMemo(() => {
    const pages: SettingsElsewhere[] = [
      // Its machine rows only point at the machines' own pages, which are listed themselves.
      { target: "owner", title: "Agents settings", items: ownerSettingItems(machines, services).filter((i) => !i.id.startsWith("machine.")) },
      ...machines.map((m) => ({ target: `m:${m.id}`, title: m.name, items: machineSettingItems(m.info?.settings ?? null, !!m.sandbox) })),
    ];
    return pages.filter((p) => p.target !== target);
  }, [machines, services, target]);
}

function Gone({ onClose }: { onClose: () => void }) {
  return (
    <SettingsPage icon={null} title="Not here any more" subtitle="It was removed" onClose={onClose} items={[]} search={false}>
      <p className="text-[13px] text-muted">Pick a machine or project on the canvas.</p>
    </SettingsPage>
  );
}

function ProjectOverview({ project, machine }: { project: Project; machine: Machine | undefined }) {
  const rows = [
    { id: "project.folder", title: "Folder", value: shortPath(project.path, machine?.info?.home), full: project.path },
    project.repo && { id: "project.repo", title: "Repository", value: `${project.repo.owner}/${project.repo.repo}` },
    project.git?.branch && { id: "project.branch", title: "Branch", value: project.git.branch },
  ].filter((r) => !!r);
  return (
    <SettingsSection title="Project" ids={rows.map((r) => r.id)}>
      {rows.map((r) => (
        <SettingRow
          key={r.id}
          s={{ id: r.id, title: r.title, section: "Project" }}
          control={
            <span className="font-mono text-[12px] text-muted" title={"full" in r ? r.full : undefined}>
              {r.value}
            </span>
          }
        />
      ))}
    </SettingsSection>
  );
}
