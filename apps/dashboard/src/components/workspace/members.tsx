import { Check, Copy, LogOut, Trash2, UserPlus, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useSession } from "@/lib/accounts-api";
import { ROLES, workspaceApi, type Invite, type Member, type Role, type RoleRules, type WorkspaceProject } from "@/lib/workspace-api";
import { secretsApi } from "@/lib/secrets-api";
import { controlsFor, describeEnvironments, developerReach, grantable, manages, parseEnvironments, type ReachedEnvironment } from "@/lib/workspace-members";
import { btn, danger, ErrorNote, input, panel, primary, useCopy } from "../ui";

const ROLE_NOTE: Record<Role, string> = {
  viewer: "Names and history, never a value",
  developer: "Reads and writes values where the role applies",
  admin: "Everything in the project, and its members",
  owner: "As admin, plus owners and deleting the project",
};

const when = (ms: number) => new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });

/**
 * A project's members (docs/members.md): who is in it, with which role, and pending invites.
 * Controls a member can't use are hidden (`lib/workspace-members.ts`); rolld decides anyway.
 */
export function ProjectMembers({ project, onLeft }: { project: WorkspaceProject; onLeft?: () => void }) {
  const session = useSession();
  const self = session.status === "signed_in" && !session.user.admin ? session.user.id : undefined;
  const me: Role | undefined = project.role;
  const [members, setMembers] = useState<Member[] | null>(null);
  const [invites, setInvites] = useState<Invite[]>([]);
  const [rules, setRules] = useState<RoleRules | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [inviting, setInviting] = useState(false);
  // The project's environments, for the invite sheet to say what a developer will reach.
  const [envs, setEnvs] = useState<ReachedEnvironment[] | null>(null);
  const [link, setLink] = useState<{ email: string; url: string } | null>(null);
  const { copied, copy } = useCopy();

  const load = useCallback(() => {
    workspaceApi.members(project.id).then(setMembers, (e: Error) => setError(e.message));
    if (manages(me)) workspaceApi.invites(project.id).then(setInvites, () => setInvites([]));
  }, [project.id, me]);
  useEffect(load, [load]);
  useEffect(() => {
    workspaceApi.roles().then(setRules, () => setRules(null));
  }, []);
  useEffect(() => {
    if (inviting) secretsApi(project.id).environments().then(setEnvs, () => setEnvs(null));
  }, [inviting, project.id]);

  const owners = useMemo(() => (members ?? []).filter((m) => m.role === "owner" && !m.inactive).length, [members]);
  const act = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  return (
    <div className="mx-auto flex w-full max-w-4xl flex-col gap-4 p-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-[18px] font-semibold tracking-[-0.04em]">Members</h1>
          <p className="text-[13px] text-muted">
            {me ? (
              <>
                You're {me === "admin" || me === "owner" ? "an" : "a"} <span className="text-fg">{me}</span> here. {ROLE_NOTE[me]}.
              </>
            ) : (
              "You manage this project as roll's operator."
            )}
          </p>
        </div>
        {manages(me) && !inviting && (
          <button className={primary} onClick={() => setInviting(true)}>
            <UserPlus className="size-3.5" strokeWidth={1.5} />
            Invite
          </button>
        )}
      </div>

      <ErrorNote error={error} />

      {inviting && (
        <InviteForm
          roles={grantable(me)}
          def={rules?.developer_default ?? "unprotected"}
          envs={envs}
          onCancel={() => setInviting(false)}
          onInvite={async (email, role, envs) => {
            setError(null);
            try {
              const inv = await workspaceApi.invite(project.id, email, role, envs);
              setLink({ email: inv.email, url: inv.url });
              setInviting(false);
              load();
            } catch (e) {
              setError((e as Error).message);
            }
          }}
        />
      )}

      {link && (
        <div className="rise flex flex-wrap items-center gap-2 rounded-xl border border-line bg-panel px-4 py-3 text-[13px]">
          <span className="text-muted">
            Invited <span className="text-fg">{link.email}</span>. The link works once, for 7 days, for that address only:
          </span>
          <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{link.url}</code>
          <button className={btn} onClick={() => copy("link", link.url)} aria-label="Copy the invite link">
            {copied === "link" ? <Check className="size-3.5" strokeWidth={1.5} /> : <Copy className="size-3.5" strokeWidth={1.5} />}
          </button>
          <button className={btn} onClick={() => setLink(null)} aria-label="Dismiss">
            <X className="size-3.5" strokeWidth={1.5} />
          </button>
        </div>
      )}

      <section className={`${panel} overflow-hidden`}>
        <div className="hidden grid-cols-[minmax(0,1.6fr)_7rem_minmax(0,1fr)_7rem_auto] gap-3 border-b border-line px-4 py-2 text-[11px] uppercase tracking-wide text-subtle md:grid">
          <span>Person</span>
          <span>Role</span>
          <span>Environments</span>
          <span>Added</span>
          <span className="w-16" />
        </div>
        {members === null ? (
          <div className="shimmer px-4 py-6 text-[13px] text-muted">Loading members…</div>
        ) : members.length === 0 ? (
          <div className="px-4 py-6 text-[13px] text-muted">No members yet. {manages(me) ? "Invite someone to share this project." : ""}</div>
        ) : (
          <ul className="divide-y divide-line">
            {members.map((m) => (
              <MemberRow
                key={m.user}
                m={m}
                self={self}
                me={me}
                def={rules?.developer_default ?? "unprotected"}
                controls={controlsFor(me, self, m, owners)}
                onRole={(role, envs) => act(() => workspaceApi.setRole(project.id, m.user, role, envs))}
                onRemove={() => act(() => workspaceApi.removeMember(project.id, m.user))}
                onLeave={() =>
                  act(async () => {
                    await workspaceApi.leave(project.id);
                    onLeft?.();
                  })
                }
              />
            ))}
          </ul>
        )}
      </section>

      {manages(me) && invites.length > 0 && (
        <section className={`${panel} overflow-hidden`}>
          <h2 className="border-b border-line px-4 py-2.5 text-[13px] font-medium">Pending invites</h2>
          <ul className="divide-y divide-line">
            {invites.map((i) => (
              <li key={i.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2.5 text-[13px]">
                <span className="min-w-0 flex-1 truncate">{i.email}</span>
                <span className="text-muted">{i.role}</span>
                <span className="font-mono text-[12px] text-subtle">expires {when(i.expires_at)}</span>
                {grantable(me).includes(i.role) && (
                  <button className={btn} onClick={() => act(() => workspaceApi.revokeInvite(project.id, i.id))}>
                    Revoke
                  </button>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

function MemberRow({
  m,
  self,
  me,
  def,
  controls,
  onRole,
  onRemove,
  onLeave,
}: {
  m: Member;
  self: string | undefined;
  me: Role | undefined;
  def: "unprotected" | "all" | "none";
  controls: ReturnType<typeof controlsFor>;
  onRole: (role: Role, envs: string[] | null) => void;
  onRemove: () => void;
  onLeave: () => void;
}) {
  const [confirm, setConfirm] = useState<"remove" | "leave" | null>(null);
  const [envs, setEnvs] = useState((m.environments ?? []).join(", "));
  const options = grantable(me);
  return (
    <li className="px-4 py-3">
      <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 md:grid-cols-[minmax(0,1.6fr)_7rem_minmax(0,1fr)_7rem_auto]">
        <div className="min-w-0">
          <div className="truncate text-[13px] font-medium">
            {m.handle}
            {m.user === self && <span className="ml-1.5 text-subtle">(you)</span>}
            {m.inactive && <span className="ml-1.5 text-[12px] text-subtle">can't sign in</span>}
          </div>
          {m.email && <div className="truncate text-[12px] text-subtle">{m.email}</div>}
        </div>
        <div className="max-md:order-3 max-md:col-span-2 max-md:flex max-md:gap-3">
          {controls.changeRole ? (
            <select
              aria-label={`${m.handle}'s role`}
              value={m.role}
              onChange={(e) => onRole(e.target.value as Role, e.target.value === "developer" ? parseEnvironments(envs) : null)}
              className="h-7 rounded-md border border-line bg-panel px-1.5 text-[12px] outline-none"
            >
              {ROLES.filter((r) => options.includes(r)).map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
          ) : (
            <span className="text-[13px] text-muted">{m.role}</span>
          )}
        </div>
        <div className="min-w-0 max-md:order-4 max-md:col-span-2">
          {controls.changeRole && m.role === "developer" ? (
            <input
              aria-label={`${m.handle}'s environments`}
              value={envs}
              placeholder={def === "unprotected" ? "every unprotected one" : def}
              onChange={(e) => setEnvs(e.target.value)}
              onBlur={() => {
                const next = parseEnvironments(envs);
                if (JSON.stringify(next) !== JSON.stringify(m.environments)) onRole("developer", next);
              }}
              className={`${input} h-7 w-full py-0 font-mono text-[12px]`}
            />
          ) : (
            <span className="truncate font-mono text-[12px] text-muted">{describeEnvironments(m, def)}</span>
          )}
        </div>
        <span className="font-mono text-[12px] text-subtle max-md:hidden">{when(m.added_at)}</span>
        <div className="flex w-16 justify-end gap-1 max-md:order-2">
          {controls.remove && (
            <button className={btn} onClick={() => setConfirm("remove")} aria-label={`Remove ${m.handle}`} title="Remove">
              <Trash2 className="size-3.5" strokeWidth={1.5} />
            </button>
          )}
          {controls.leave && (
            <button className={btn} onClick={() => setConfirm("leave")} aria-label="Leave the project" title="Leave">
              <LogOut className="size-3.5" strokeWidth={1.5} />
            </button>
          )}
        </div>
      </div>
      {confirm && (
        <div className="rise mt-3 flex flex-wrap items-center gap-2 rounded-lg border border-danger/40 bg-danger/5 px-3 py-2 text-[13px]">
          <span className="min-w-0 flex-1 text-muted">
            {confirm === "remove"
              ? `${m.handle} stops reaching this project at once, and their tokens confined to it are revoked.`
              : "You stop reaching this project at once, and your tokens confined to it are revoked."}
          </span>
          <button className={danger} onClick={() => (confirm === "remove" ? onRemove() : onLeave())}>
            {confirm === "remove" ? "Remove" : "Leave"}
          </button>
          <button className={btn} onClick={() => setConfirm(null)}>
            Cancel
          </button>
        </div>
      )}
    </li>
  );
}

function InviteForm({
  roles,
  def,
  envs,
  onInvite,
  onCancel,
}: {
  roles: Role[];
  def: "unprotected" | "all" | "none";
  envs: ReachedEnvironment[] | null;
  onInvite: (email: string, role: Role, envs: string[] | null) => Promise<void>;
  onCancel: () => void;
}) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<Role>(roles.includes("developer") ? "developer" : roles[0]!);
  const [named, setNamed] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <form
      className={`${panel} rise flex flex-col gap-3 p-4`}
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        await onInvite(email.trim(), role, role === "developer" ? parseEnvironments(named) : null);
        setBusy(false);
      }}
    >
      <h2 className="text-[14px] font-medium">Invite someone</h2>
      <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_10rem]">
        <label className="flex flex-col gap-1">
          <span className="text-[11px] text-subtle">Email</span>
          <input autoFocus type="email" required value={email} onChange={(e) => setEmail(e.target.value)} placeholder="name@company.com" className={input} />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-[11px] text-subtle">Role</span>
          <select value={role} onChange={(e) => setRole(e.target.value as Role)} className={`${input} h-[34px]`}>
            {roles.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>
        </label>
      </div>
      <p className="text-[12px] text-muted">{ROLE_NOTE[role]}.</p>
      {role === "developer" && (
        <label className="flex flex-col gap-1">
          <span className="text-[11px] text-subtle">Environments, comma separated</span>
          <input value={named} onChange={(e) => setNamed(e.target.value)} placeholder={def === "unprotected" ? "Leave empty: every environment that isn't protected, never production" : def} className={`${input} font-mono text-[12px]`} />
        </label>
      )}
      {role === "developer" && envs && <Reach slugs={developerReach(envs, parseEnvironments(named), def)} />}
      <div className="flex gap-2">
        <button type="submit" disabled={busy || !email.trim()} className={primary}>
          {busy ? <span className="shimmer">Inviting…</span> : "Send invite"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** What a developer invited this way reads and writes values in. */
function Reach({ slugs }: { slugs: string[] }) {
  return (
    <p className="text-[12px] text-muted" role="status">
      {slugs.length ? (
        <>
          They'll read and write values in <span className="font-mono text-fg">{slugs.join(", ")}</span>
          {slugs.includes("production") ? "." : "; production only if you name it."}
        </>
      ) : (
        "They'll see names only: no environment here matches."
      )}
    </p>
  );
}
