"use client";

import { Folder, X } from "lucide-react";
import type { ReactNode } from "react";
import type { Machine, Project } from "@/lib/agents-api";
import { ProjectSettingsForm } from "./project-settings";
import { MachineIcon, shortPath } from "./shared";
import { IconButton } from "./session";
import { useAgents } from "./use-agents";

/**
 * A machine's or a project's settings in the side panel beside the canvas
 * (`{ kind: "settings", target }`, `#agents/settings/<key>`): the card stays on the canvas,
 * selected, and the panel holds everything that can be changed about it.
 */
export function SettingsPanel({ target, onClose }: { target: string; onClose: () => void }) {
  const { client } = useAgents();
  const data = client.data;
  const id = target.slice(2);
  if (target.startsWith("m:")) {
    const machine = data.machines.get(id)?.value;
    if (!machine) return <Gone onClose={onClose} />;
    return (
      <Shell icon={<MachineIcon machine={machine} className="size-4 text-muted" />} title={machine.name} subtitle="Machine settings" onClose={onClose}>
        <MachineSettings machine={machine} />
      </Shell>
    );
  }
  if (target.startsWith("p:")) {
    const r = data.projects.get(id);
    if (!r) return <Gone onClose={onClose} />;
    return (
      <Shell icon={<Folder className="size-4 text-muted" />} title={r.value.name} subtitle="Project settings" onClose={onClose}>
        <ProjectOverview project={r.value} machine={data.machines.get(r.machine)?.value} />
        <ProjectSettingsForm project={r.value} machine={data.machines.get(r.machine)?.value} />
      </Shell>
    );
  }
  return <Gone onClose={onClose} />;
}

function Shell({ icon, title, subtitle, onClose, children }: { icon: ReactNode; title: string; subtitle: string; onClose: () => void; children: ReactNode }) {
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-12 shrink-0 items-center gap-2.5 border-b border-line px-3">
        {icon}
        <div className="min-w-0 flex-1 leading-tight">
          <p className="truncate text-[13px] font-medium tracking-[-0.01em]">{title}</p>
          <p className="text-[11px] text-subtle">{subtitle}</p>
        </div>
        <IconButton label="Close (Esc)" onClick={onClose}>
          <X className="size-4" />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4" data-scroll>
        {children}
      </div>
    </div>
  );
}

function Gone({ onClose }: { onClose: () => void }) {
  return (
    <Shell icon={null} title="Not here any more" subtitle="It was removed" onClose={onClose}>
      <p className="text-[13px] text-muted">Pick a machine or project on the canvas.</p>
    </Shell>
  );
}

/** A section of settings: a heading and its rows. */
export function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="mb-6">
      <h3 className="mb-2 text-[11px] font-medium tracking-wide text-subtle uppercase">{title}</h3>
      <div className="divide-y divide-line rounded-lg border border-line bg-panel">{children}</div>
    </section>
  );
}

export function Row({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="flex items-center gap-3 px-3 py-2.5 text-[13px]">
      <div className="min-w-0 flex-1">
        <p>{label}</p>
        {hint && <p className="mt-0.5 text-[11.5px] text-subtle">{hint}</p>}
      </div>
      <div className="min-w-0 shrink-0 text-right text-muted">{children}</div>
    </div>
  );
}

function MachineSettings({ machine }: { machine: Machine }) {
  const info = machine.info;
  return (
    <>
      <Section title="Machine">
        <Row label="Status">{machine.online ? "Online" : "Offline"}</Row>
        {info && (
          <Row label="System">
            <span className="font-mono text-[12px]">
              {info.os} · {info.arch} · roll {info.version}
            </span>
          </Row>
        )}
      </Section>
      <Section title="Agents">
        {(info?.providers ?? []).map((p) => (
          <Row key={p.id} label={p.name} hint={p.models.length ? `${p.models.length} models` : undefined}>
            {!p.installed ? "Not installed" : p.auth !== "missing" ? (p.version ?? "Ready") : "Needs a login"}
          </Row>
        ))}
      </Section>
    </>
  );
}

function ProjectOverview({ project, machine }: { project: Project; machine: Machine | undefined }) {
  return (
    <Section title="Project">
      <Row label="Folder">
        <span className="font-mono text-[12px]" title={project.path}>
          {shortPath(project.path, machine?.info?.home)}
        </span>
      </Row>
      {project.repo && (
        <Row label="Repository">
          <span className="font-mono text-[12px]">
            {project.repo.owner}/{project.repo.repo}
          </span>
        </Row>
      )}
      {project.git?.branch && (
        <Row label="Branch">
          <span className="font-mono text-[12px]">{project.git.branch}</span>
        </Row>
      )}
    </Section>
  );
}
