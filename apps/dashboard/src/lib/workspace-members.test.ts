import { describe, expect, it } from "vitest";
import type { Member, Role } from "./workspace-api";
import { controlsFor, describeEnvironments, developerReach, grantable, parseEnvironments } from "./workspace-members";

const member = (user: string, role: Role): Member => ({ project: "prj_a", user, handle: user.slice(4), role, environments: null, added_by: "usr_ada", added_at: 0 });

describe("the members page offers what each role may do", () => {
  it("lets nobody grant past their own role", () => {
    expect(grantable(undefined)).toEqual(["viewer", "developer", "admin", "owner"]);
    expect(grantable("owner")).toEqual(["viewer", "developer", "admin", "owner"]);
    expect(grantable("admin")).toEqual(["viewer", "developer", "admin"]);
    expect(grantable("developer")).toEqual([]);
    expect(grantable("viewer")).toEqual([]);
  });

  it("hides changes to owners from admins, and the last owner's exits from everyone", () => {
    const owner = member("usr_ada", "owner");
    const dev = member("usr_bob", "developer");
    expect(controlsFor("admin", "usr_cat", owner, 2)).toEqual({ changeRole: false, remove: false, leave: false });
    expect(controlsFor("admin", "usr_cat", dev, 1)).toEqual({ changeRole: true, remove: true, leave: false });
    expect(controlsFor("owner", "usr_ada", owner, 1)).toEqual({ changeRole: false, remove: false, leave: false });
    expect(controlsFor("owner", "usr_ada", owner, 2)).toEqual({ changeRole: true, remove: false, leave: true });
    expect(controlsFor("developer", "usr_bob", dev, 1)).toEqual({ changeRole: false, remove: false, leave: true });
    expect(controlsFor("viewer", "usr_vic", dev, 1)).toEqual({ changeRole: false, remove: false, leave: false });
    expect(controlsFor(undefined, undefined, owner, 1).remove).toBe(false);
  });

  it("reads environments as the server takes them", () => {
    expect(parseEnvironments(" staging, production ,")).toEqual(["staging", "production"]);
    expect(parseEnvironments("  ")).toBeNull();
    expect(describeEnvironments({ role: "developer", environments: null })).toBe("unprotected");
    expect(describeEnvironments({ role: "developer", environments: ["production"] })).toBe("production");
    expect(describeEnvironments({ role: "admin", environments: null })).toBe("all");
  });

  it("says which environments a developer will reach: never production unless named", () => {
    const env = (slug: string, p = false) => ({ slug, kind: "standard" as const, protected: p });
    // A project made before production started protected: still left out.
    const envs = [env("development"), env("staging"), env("production"), env("qa", true), { slug: "pv-1", kind: "ephemeral" as const, protected: false }];
    expect(developerReach(envs, null)).toEqual(["development", "staging"]);
    expect(developerReach(envs, ["production", "q*"])).toEqual(["production", "qa"]);
    expect(developerReach(envs, null, "none")).toEqual([]);
  });
});
