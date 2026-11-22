/**
 * Which member controls the Members tab offers (docs/members.md, "Dashboard"): read from the
 * caller's role, never guessed. rolld decides anyway; this only hides what would be refused.
 *
 * The rule is the server's: managing members is `admin` on the project, and a role is given or
 * taken away only when the caller's own rules cover it (an admin manages up to admin; only an
 * owner makes, changes or removes owners). The operator and roll's admins (no role) manage all.
 */

import { GROUP_ROLES, ROLES, type GroupRole, type GroupRoleName, type Member, type Role } from "./workspace-api";

const rank = (r: Role) => ROLES.indexOf(r);

/** Whether a caller with `me` (none: the operator) manages the project's members. */
export function manages(me: Role | undefined): boolean {
  return me === undefined || me === "admin" || me === "owner";
}

/** The roles a caller with `me` may give: up to their own (all for the operator). */
export function grantable(me: Role | undefined): Role[] {
  if (!manages(me)) return [];
  return ROLES.filter((r) => me === undefined || rank(r) <= rank(me!));
}

export interface MemberControls {
  changeRole: boolean;
  remove: boolean;
  leave: boolean;
}

/** What `me` (the caller's role; `self` their own user id) may do to `m`. */
export function controlsFor(me: Role | undefined, self: string | undefined, m: Member, owners: number): MemberControls {
  const mine = self !== undefined && m.user === self;
  // The last owner stays an owner (409 `last_owner`): offering it would only be refused.
  const lastOwner = m.role === "owner" && owners <= 1;
  const within = grantable(me).includes(m.role);
  return {
    changeRole: within && !lastOwner,
    remove: within && !mine && !lastOwner,
    leave: mine && !lastOwner,
  };
}

/** A developer's environments as the form takes them: comma separated; empty is the default. */
export function parseEnvironments(text: string): string[] | null {
  const list = text
    .split(",")
    .map((e) => e.trim())
    .filter(Boolean);
  return list.length ? list : null;
}

/** How a member's environments read: their list, or what the default is. */
export function describeEnvironments(m: Pick<Member, "role" | "environments">, def: "unprotected" | "all" | "none" = "unprotected"): string {
  if (m.role !== "developer") return "all";
  if (m.environments?.length) return m.environments.join(", ");
  return def === "unprotected" ? "unprotected" : def;
}

/** The roles a caller with `me` may give a group: up to their own, never owner (groups never hold it). */
export function groupGrantable(me: Role | undefined): GroupRoleName[] {
  const mine = grantable(me);
  return GROUP_ROLES.filter((r) => mine.includes(r));
}

/**
 * What `me` may do to group role `g`: change or remove it when its role is theirs to give. Giving
 * a group a role also takes being in it (`inGroups`: the ids of the caller's groups; `undefined`
 * for the operator, who may give any group one).
 */
export function groupControlsFor(me: Role | undefined, g: GroupRole, inGroups: Set<string> | undefined): { change: boolean; remove: boolean } {
  const within = groupGrantable(me).includes(g.role);
  return { change: within && (inGroups === undefined || inGroups.has(g.group)), remove: within };
}
