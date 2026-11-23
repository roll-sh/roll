import { Trash2, Users } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { workspaceApi, type Group, type GroupRole, type GroupRoleName, type Role, type WorkspaceProject } from "@/lib/workspace-api";
import { describeEnvironments, groupControlsFor, groupGrantable, manages, parseEnvironments } from "@/lib/workspace-members";
import { btn, danger, ErrorNote, input, panel, primary } from "../ui";

const when = (ms: number) => new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });

/**
 * The groups with a role on a project (docs/members-v1-1.md): every person in a group gets its
 * role here, on top of their own. A group never holds owner. Giving one a role takes being in it,
 * unless you're roll's operator. Controls are hidden by `lib/workspace-members.ts`; rolld decides anyway.
 */
export function ProjectGroups({ project, def }: { project: WorkspaceProject; def: "unprotected" | "all" | "none" }) {
  const me: Role | undefined = project.role;
  const [roles, setRoles] = useState<GroupRole[] | null>(null);
  const [groups, setGroups] = useState<Group[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  const load = useCallback(() => {
    workspaceApi.projectGroups(project.id).then(setRoles, (e: Error) => setError(e.message));
    if (manages(me)) workspaceApi.groups().then(setGroups, () => setGroups([]));
  }, [project.id, me]);
  useEffect(load, [load]);

  // The operator gives any group a role; a person only those they're in (D11).
  const inGroups = useMemo(() => (me === undefined ? undefined : new Set(groups.filter((g) => g.yours).map((g) => g.id))), [groups, me]);
  const offered = useMemo(() => {
    const held = new Set((roles ?? []).map((r) => r.group));
    return groups.filter((g) => !held.has(g.id) && (inGroups === undefined || inGroups.has(g.id)));
  }, [groups, roles, inGroups]);
  const act = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  if (roles !== null && roles.length === 0 && !(manages(me) && offered.length > 0)) return null;
  return (
    <section className={`${panel} overflow-hidden`}>
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-2.5">
        <div>
          <h2 className="text-[13px] font-medium">Groups</h2>
          <p className="text-[12px] text-muted">Everyone in a group gets its role here, on top of their own.</p>
        </div>
        {manages(me) && offered.length > 0 && !adding && (
          <button className={btn} onClick={() => setAdding(true)}>
            <Users className="size-3.5" strokeWidth={1.5} />
            Give a group a role
          </button>
        )}
      </div>
      <div className="px-4 empty:hidden">
        <ErrorNote error={error} />
      </div>
      {adding && (
        <GroupRoleForm
          groups={offered}
          roles={groupGrantable(me)}
          def={def}
          onCancel={() => setAdding(false)}
          onGive={(group, role, envs) =>
            act(async () => {
              await workspaceApi.setGroupRole(project.id, group, role, envs);
              setAdding(false);
            })
          }
        />
      )}
      {roles === null ? (
        <div className="shimmer px-4 py-6 text-[13px] text-muted">Loading groups…</div>
      ) : (
        <ul className="divide-y divide-line">
          {roles.map((g) => (
            <GroupRow
              key={g.group}
              g={g}
              def={def}
              options={groupGrantable(me)}
              controls={groupControlsFor(me, g, inGroups)}
              onRole={(role, envs) => act(() => workspaceApi.setGroupRole(project.id, g.group, role, envs))}
              onRemove={() => act(() => workspaceApi.removeGroupRole(project.id, g.group))}
            />
          ))}
        </ul>
      )}
    </section>
  );
}

function GroupRow({
  g,
  def,
  options,
  controls,
  onRole,
  onRemove,
}: {
  g: GroupRole;
  def: "unprotected" | "all" | "none";
  options: GroupRoleName[];
  controls: { change: boolean; remove: boolean };
  onRole: (role: GroupRoleName, envs: string[] | null) => void;
  onRemove: () => void;
}) {
  const [confirm, setConfirm] = useState(false);
  const [envs, setEnvs] = useState((g.environments ?? []).join(", "));
  const scim = g.source.startsWith("scim:");
  return (
    <li className="px-4 py-3">
      <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 md:grid-cols-[minmax(0,1.6fr)_7rem_minmax(0,1fr)_7rem_auto]">
        <div className="min-w-0">
          <div className="truncate text-[13px] font-medium">{g.name}</div>
          <div className="truncate text-[12px] text-subtle">
            <span className="font-mono">{g.members}</span> {g.members === 1 ? "person" : "people"}
            {scim && " · kept by your identity provider"}
          </div>
        </div>
        <div className="max-md:order-3 max-md:col-span-2">
          {controls.change ? (
            <select
              aria-label={`${g.name}'s role`}
              value={g.role}
              onChange={(e) => onRole(e.target.value as GroupRoleName, e.target.value === "developer" ? parseEnvironments(envs) : null)}
              className="h-7 rounded-md border border-line bg-panel px-1.5 text-[12px] outline-none"
            >
              {options.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
          ) : (
            <span className="text-[13px] text-muted">{g.role}</span>
          )}
        </div>
        <div className="min-w-0 max-md:order-4 max-md:col-span-2">
          {controls.change && g.role === "developer" ? (
            <input
              aria-label={`${g.name}'s environments`}
              value={envs}
              placeholder={def === "unprotected" ? "every unprotected one" : def}
              onChange={(e) => setEnvs(e.target.value)}
              onBlur={() => {
                const next = parseEnvironments(envs);
                if (JSON.stringify(next) !== JSON.stringify(g.environments)) onRole("developer", next);
              }}
              className={`${input} h-7 w-full py-0 font-mono text-[12px]`}
            />
          ) : (
            <span className="truncate font-mono text-[12px] text-muted">{describeEnvironments(g, def)}</span>
          )}
        </div>
        <span className="font-mono text-[12px] text-subtle max-md:hidden">{when(g.added_at)}</span>
        <div className="flex w-16 justify-end gap-1 max-md:order-2">
          {controls.remove && (
            <button className={btn} onClick={() => setConfirm(true)} aria-label={`Take ${g.name}'s role away`} title="Remove">
              <Trash2 className="size-3.5" strokeWidth={1.5} />
            </button>
          )}
        </div>
      </div>
      {confirm && (
        <div className="rise mt-3 flex flex-wrap items-center gap-2 rounded-lg border border-danger/40 bg-danger/5 px-3 py-2 text-[13px]">
          <span className="min-w-0 flex-1 text-muted">Everyone in {g.name} loses this role at once. Those not in the project another way lose it entirely, and their tokens confined to it are revoked.</span>
          <button className={danger} onClick={onRemove}>
            Remove
          </button>
          <button className={btn} onClick={() => setConfirm(false)}>
            Cancel
          </button>
        </div>
      )}
    </li>
  );
}

function GroupRoleForm({
  groups,
  roles,
  def,
  onGive,
  onCancel,
}: {
  groups: Group[];
  roles: GroupRoleName[];
  def: "unprotected" | "all" | "none";
  onGive: (group: string, role: GroupRoleName, envs: string[] | null) => Promise<void>;
  onCancel: () => void;
}) {
  const [group, setGroup] = useState(groups[0]?.id ?? "");
  const [role, setRole] = useState<GroupRoleName>(roles.includes("developer") ? "developer" : roles[0]!);
  const [envs, setEnvs] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="rise flex flex-col gap-3 border-b border-line p-4"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        await onGive(group, role, role === "developer" ? parseEnvironments(envs) : null);
        setBusy(false);
      }}
    >
      <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_10rem]">
        <label className="flex flex-col gap-1">
          <span className="text-[11px] text-subtle">Group</span>
          <select value={group} onChange={(e) => setGroup(e.target.value)} className={`${input} h-[34px]`}>
            {groups.map((g) => (
              <option key={g.id} value={g.id}>
                {g.name} ({g.members})
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-[11px] text-subtle">Role</span>
          <select value={role} onChange={(e) => setRole(e.target.value as GroupRoleName)} className={`${input} h-[34px]`}>
            {roles.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>
        </label>
      </div>
      {role === "developer" && (
        <label className="flex flex-col gap-1">
          <span className="text-[11px] text-subtle">Environments, comma separated</span>
          <input value={envs} onChange={(e) => setEnvs(e.target.value)} placeholder={def === "unprotected" ? "Leave empty: every environment that isn't protected" : def} className={`${input} font-mono text-[12px]`} />
        </label>
      )}
      <div className="flex gap-2">
        <button type="submit" disabled={busy || !group} className={primary}>
          {busy ? <span className="shimmer">Giving…</span> : "Give the role"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}
