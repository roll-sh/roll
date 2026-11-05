"use client";

import type { Machine, Project } from "@/lib/agents-api";
import { ProjectIcon } from "./identity";
import { MachineSettingsPanel } from "./machine-settings";
import { OwnerSettingsPanel } from "./owner-settings-panel";
import { ProjectSettingsForm } from "./project-settings";
import { SettingRow, SettingsPage, SettingsSection } from "./settings-ui";
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
  if (target === "owner") return <OwnerSettingsPanel onClose={onClose} />;
  const id = target.slice(2);
  if (target.startsWith("m:")) {
    const machine = data.machines.get(id)?.value;
    return machine ? <MachineSettingsPanel machine={machine} onClose={onClose} /> : <Gone onClose={onClose} />;
  }
  if (target.startsWith("p:")) {
    const r = data.projects.get(id);
    if (!r) return <Gone onClose={onClose} />;
    const machine = data.machines.get(r.machine)?.value;
    return (
      <SettingsPage icon={<ProjectIcon project={r.value} className="size-4" />} title={r.value.name} subtitle="Project settings" onClose={onClose} items={[]} search={false}>
        <ProjectOverview project={r.value} machine={machine} />
        <ProjectSettingsForm project={r.value} machine={machine} />
      </SettingsPage>
    );
  }
  return <Gone onClose={onClose} />;
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
