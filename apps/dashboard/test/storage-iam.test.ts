// IAM through the control plane (src/lib/storage-iam.ts): the request shape (`POST /v1/storage/iam`
// with `Action`), reading IAM's JSON (lists as `null` / arrays, leaves as strings), and the pure
// helpers behind the IAM pages. `fetch` is stubbed.

import assert from "node:assert/strict";
import { afterEach, beforeEach, describe, it } from "node:test";
import {
  arnName,
  bool,
  cleanCode,
  contextEntries,
  credentialReport,
  decisionLabel,
  decodeBase64Text,
  generatePassword,
  iam,
  iamAll,
  iamApi,
  iamBody,
  iamDate,
  iamNameError,
  iamPathError,
  iamTagsError,
  isAwsManaged,
  members,
  mfaCodesError,
  mfaSeed,
  num,
  parseCsv,
  passwordPolicyDraft,
  passwordPolicyError,
  passwordPolicyParams,
  passwordProblems,
  PASSWORD_SYMBOLS,
  policyDocumentError,
  prettyPolicy,
  principalArn,
  qrDataUrl,
  reportCell,
  simulationError,
  simulationRequest,
  sourceDocument,
  splitList,
  starterPolicy,
  starterTrustPolicy,
  statementLines,
  statementRange,
  summaryStats,
  type SimulationDraft,
} from "../src/lib/storage-iam";
import { StorageError } from "../src/lib/storage-session";

interface Call {
  method: string;
  path: string;
  body: Record<string, unknown>;
}

const json = (v: unknown, status = 200) => new Response(JSON.stringify(v), { status, headers: { "content-type": "application/json" } });
const realFetch = globalThis.fetch;
let calls: Call[] = [];
let reply: (c: Call) => Response = () => json({});

beforeEach(() => {
  calls = [];
  reply = () => json({});
  globalThis.fetch = (async (input: string | URL | Request, init: RequestInit = {}) => {
    const c: Call = { method: init.method ?? "GET", path: new URL(String(input)).pathname, body: JSON.parse(String(init.body ?? "{}")) };
    calls.push(c);
    return reply(c);
  }) as typeof fetch;
});
afterEach(() => {
  globalThis.fetch = realFetch;
});

const b64 = (s: string) => Buffer.from(s, "utf8").toString("base64");

describe("requests", () => {
  it("POST /v1/storage/iam with the Action and the params as JSON", async () => {
    await iam("CreateUser", { UserName: "alice", Tags: [{ Key: "team", Value: "web" }] });
    assert.deepEqual(calls, [{ method: "POST", path: "/v1/storage/iam", body: { Action: "CreateUser", UserName: "alice", Tags: [{ Key: "team", Value: "web" }] } }]);
  });

  it("empty params are left out", () => {
    assert.deepEqual(iamBody("ListPolicies", { Scope: "Local", OnlyAttached: undefined, PathPrefix: "", Marker: null, ResourceArns: [], Flag: false, N: 0 }), {
      Action: "ListPolicies",
      Scope: "Local",
      Flag: false,
      N: 0,
    });
  });

  it("errors carry IAM's code", async () => {
    reply = () => json({ error: "Syntax errors in policy.", code: "MalformedPolicyDocument" }, 400);
    await assert.rejects(iam("CreatePolicy", { PolicyName: "p", PolicyDocument: "{}" }), (e: unknown) => {
      assert.ok(e instanceof StorageError);
      assert.equal(e.code, "MalformedPolicyDocument");
      assert.equal(e.message, "Syntax errors in policy.");
      return true;
    });
  });

  it("iamAll follows Marker while IsTruncated", async () => {
    reply = (c) =>
      c.body.Marker === undefined
        ? json({ Users: [{ UserName: "a" }], IsTruncated: "true", Marker: "m1" })
        : c.body.Marker === "m1"
          ? json({ Users: { UserName: "b" }, IsTruncated: "true", Marker: "m2" })
          : json({ Users: null, IsTruncated: "false" });
    const users = await iamAll<{ UserName: string }>("ListUsers", "Users");
    assert.deepEqual(
      users.map((u) => u.UserName),
      ["a", "b"],
    );
    assert.deepEqual(
      calls.map((c) => [c.body.Marker, c.body.MaxItems]),
      [
        [undefined, 1000],
        ["m1", 1000],
        ["m2", 1000],
      ],
    );
  });

  it("an empty list is null and reads as []", async () => {
    reply = () => json({ Users: null, IsTruncated: "false" });
    assert.deepEqual(await iamApi.listUsers(), []);
  });

  it("actions per target kind", async () => {
    await iamApi.listInline({ kind: "group", name: "devs" });
    await iamApi.putInline({ kind: "role", name: "ci" }, "p", "{}");
    await iamApi.attach({ kind: "user", name: "alice" }, "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess");
    await iamApi.listAttached({ kind: "role", name: "ci" });
    await iamApi.untag({ kind: "role", name: "ci" }, ["team"]);
    assert.deepEqual(
      calls.map((c) => c.body),
      [
        { Action: "ListGroupPolicies", GroupName: "devs", MaxItems: 1000 },
        { Action: "PutRolePolicy", RoleName: "ci", PolicyName: "p", PolicyDocument: "{}" },
        { Action: "AttachUserPolicy", UserName: "alice", PolicyArn: "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess" },
        { Action: "ListAttachedRolePolicies", RoleName: "ci", MaxItems: 1000 },
        { Action: "UntagRole", RoleName: "ci", TagKeys: ["team"] },
      ],
    );
  });

  it("missing login profile and password policy read as null; other errors throw", async () => {
    reply = () => json({ error: "nope", code: "NoSuchEntity" }, 404);
    assert.equal(await iamApi.getLoginProfile("alice"), null);
    assert.equal(await iamApi.getPasswordPolicy(), null);
    reply = () => json({ error: "denied", code: "AccessDenied" }, 403);
    await assert.rejects(iamApi.getPasswordPolicy(), { code: "AccessDenied" });
  });

  it("entitiesForPolicy and getGroup normalise IAM's lists", async () => {
    reply = () => json({ PolicyGroups: null, PolicyUsers: { UserName: "alice" }, PolicyRoles: [{ RoleName: "a" }, { RoleName: "b" }], IsTruncated: "false" });
    assert.deepEqual(await iamApi.entitiesForPolicy("arn"), { groups: [], users: ["alice"], roles: ["a", "b"] });
    reply = () => json({ Group: { GroupName: "g" }, Users: null });
    assert.deepEqual((await iamApi.getGroup("g")).users, []);
  });

  it("listPolicies sends Scope and OnlyAttached only when set", async () => {
    await iamApi.listPolicies("Local");
    await iamApi.listPolicies("AWS", true);
    assert.deepEqual(calls[0].body, { Action: "ListPolicies", Scope: "Local", MaxItems: 1000 });
    assert.deepEqual(calls[1].body, { Action: "ListPolicies", Scope: "AWS", OnlyAttached: true, MaxItems: 1000 });
  });

  it("MFA and passwords", async () => {
    await iamApi.enableMfaDevice("alice", "arn:aws:iam::1:mfa/alice", "123456", "654321");
    await iamApi.createLoginProfile("alice", "pw", true);
    await iamApi.changePassword("old", "new");
    assert.deepEqual(
      calls.map((c) => c.body),
      [
        { Action: "EnableMFADevice", UserName: "alice", SerialNumber: "arn:aws:iam::1:mfa/alice", AuthenticationCode1: "123456", AuthenticationCode2: "654321" },
        { Action: "CreateLoginProfile", UserName: "alice", Password: "pw", PasswordResetRequired: true },
        { Action: "ChangePassword", OldPassword: "old", NewPassword: "new" },
      ],
    );
  });

  it("summary is the SummaryMap object", async () => {
    reply = () => json({ SummaryMap: { Users: "3", UsersQuota: "5000", MFADevicesInUse: "1", AccountMFAEnabled: "0" } });
    const s = summaryStats(await iamApi.summary());
    assert.equal(s.users, 3);
    assert.equal(s.usersQuota, 5000);
    assert.equal(s.mfaDevices, 1);
    assert.equal(s.rootMfa, false);
    assert.equal(s.groups, 0);
  });
});

describe("reading IAM's JSON", () => {
  it("members", () => {
    assert.deepEqual(members(null), []);
    assert.deepEqual(members(undefined), []);
    assert.deepEqual(members(""), []);
    assert.deepEqual(members({ a: 1 }), [{ a: 1 }]);
    assert.deepEqual(members(["x"]), ["x"]);
  });

  it("bool, num, iamDate", () => {
    assert.equal(bool("true"), true);
    assert.equal(bool("false"), false);
    assert.equal(bool(true), true);
    assert.equal(bool(undefined), false);
    assert.equal(num("3600"), 3600);
    assert.equal(num(""), null);
    assert.equal(num("x"), null);
    assert.equal(num(null), null);
    assert.equal(iamDate("2026-10-22T12:00:00Z"), Date.UTC(2026, 9, 22, 12));
    assert.equal(iamDate("nope"), null);
    assert.equal(iamDate(null), null);
  });
});

describe("names, paths, tags, ARNs", () => {
  it("iamNameError", () => {
    assert.equal(iamNameError("user", "alice+ops@acme.dev"), null);
    assert.match(iamNameError("user", "")!, /Name the user/);
    assert.match(iamNameError("user", "a b")!, /letters, digits/);
    assert.match(iamNameError("role", "x".repeat(65))!, /64/);
    assert.equal(iamNameError("group", "x".repeat(128)), null);
  });

  it("iamPathError", () => {
    assert.equal(iamPathError(""), null);
    assert.equal(iamPathError("/"), null);
    assert.equal(iamPathError("/teams/web/"), null);
    assert.ok(iamPathError("teams/"));
    assert.ok(iamPathError("/teams"));
  });

  it("iamTagsError", () => {
    assert.equal(iamTagsError([{ Key: "team", Value: "web" }]), null);
    assert.match(iamTagsError([{ Key: "", Value: "x" }])!, /key/);
    assert.match(iamTagsError([{ Key: "aws:x", Value: "" }])!, /reserved/);
    assert.match(iamTagsError([{ Key: "Team", Value: "a" }, { Key: "team", Value: "b" }])!, /Duplicate/);
    assert.match(iamTagsError([{ Key: "a!", Value: "" }])!, /letters/);
    assert.match(iamTagsError(Array.from({ length: 51 }, (_, i) => ({ Key: `k${i}`, Value: "" })))!, /50/);
  });

  it("ARNs", () => {
    assert.ok(isAwsManaged("arn:aws:iam::aws:policy/AmazonS3FullAccess"));
    assert.ok(!isAwsManaged("arn:aws:iam::123456789012:policy/mine"));
    assert.equal(arnName("arn:aws:iam::123456789012:user/teams/alice"), "alice");
    assert.equal(principalArn("123456789012", "role", "ci"), "arn:aws:iam::123456789012:role/ci");
    assert.equal(principalArn("123456789012", "user", "a", "/ops/"), "arn:aws:iam::123456789012:user/ops/a");
  });
});

describe("policy documents", () => {
  it("policyDocumentError", () => {
    assert.match(policyDocumentError("")!, /Write/);
    assert.match(policyDocumentError("{")!, /Not valid JSON/);
    assert.match(policyDocumentError("[]")!, /object/);
    assert.match(policyDocumentError('{"Version":"2012-10-17"}')!, /Statement/);
    assert.match(policyDocumentError('{"Statement":"x"}')!, /Statement is/);
    assert.equal(policyDocumentError('{"Statement":[]}'), null);
  });

  it("prettyPolicy indents JSON text and keeps anything else", () => {
    assert.equal(prettyPolicy('{"a":1}'), '{\n  "a": 1\n}');
    assert.equal(prettyPolicy({ a: 1 }), '{\n  "a": 1\n}');
    assert.equal(prettyPolicy("not json"), "not json");
    assert.equal(prettyPolicy(null), "");
  });

  it("starters are valid policies", () => {
    assert.equal(policyDocumentError(starterPolicy()), null);
    const trust = JSON.parse(starterTrustPolicy("123456789012"));
    assert.equal(trust.Statement[0].Principal.AWS, "arn:aws:iam::123456789012:root");
    assert.equal(trust.Statement[0].Action, "sts:AssumeRole");
  });
});

describe("passwords", () => {
  it("IAM's default rules without a policy", () => {
    assert.deepEqual(passwordProblems("Abcdef1!", null), []);
    assert.deepEqual(passwordProblems("Abcdefg1", null), []);
    assert.deepEqual(passwordProblems("Abc1!", null), ["at least 8 characters"]);
    assert.deepEqual(passwordProblems("abcdefgh", null), ["three of: uppercase, lowercase, digits, symbols"]);
    assert.ok(passwordProblems("Aa1!".repeat(33), null).includes("at most 128 characters"));
  });

  it("the account's policy", () => {
    const policy = { MinimumPasswordLength: "12", RequireSymbols: "true", RequireNumbers: "true", RequireUppercaseCharacters: "false", RequireLowercaseCharacters: "true" };
    assert.deepEqual(passwordProblems("abcdefghijk", policy), ["at least 12 characters", "a digit", `a symbol (${PASSWORD_SYMBOLS})`]);
    assert.deepEqual(passwordProblems("abcdefghij1'", policy), []);
    assert.deepEqual(passwordProblems("abcdef", {}), []);
    assert.deepEqual(passwordProblems("abcde", {}), ["at least 6 characters"]);
  });

  it("generatePassword meets the policy", () => {
    const strict = { MinimumPasswordLength: "32", RequireSymbols: "true", RequireNumbers: "true", RequireUppercaseCharacters: "true", RequireLowercaseCharacters: "true" };
    for (let i = 0; i < 50; i++) {
      const pw = generatePassword(strict);
      assert.equal(pw.length, 32);
      assert.deepEqual(passwordProblems(pw, strict), []);
      assert.deepEqual(passwordProblems(generatePassword(null), null), []);
    }
    assert.equal(generatePassword(null).length, 20);
    // Deterministic with a fixed source.
    assert.equal(generatePassword(null, () => 0), generatePassword(null, () => 0));
  });

  it("policy draft and params", () => {
    const d = passwordPolicyDraft(null);
    assert.deepEqual(d, { min: 8, symbols: false, numbers: false, upper: false, lower: false, allowChange: true, maxAge: 0, reuse: 0, hardExpiry: false });
    assert.deepEqual(passwordPolicyParams(d), {
      MinimumPasswordLength: 8,
      RequireSymbols: false,
      RequireNumbers: false,
      RequireUppercaseCharacters: false,
      RequireLowercaseCharacters: false,
      AllowUsersToChangePassword: true,
      MaxPasswordAge: undefined,
      PasswordReusePrevention: undefined,
      HardExpiry: undefined,
    });
    const from = passwordPolicyDraft({ MinimumPasswordLength: "14", RequireSymbols: "true", AllowUsersToChangePassword: "false", MaxPasswordAge: "90", PasswordReusePrevention: "5", HardExpiry: "true" });
    assert.equal(from.min, 14);
    assert.equal(from.allowChange, false);
    const p = passwordPolicyParams(from);
    assert.equal(p.MaxPasswordAge, 90);
    assert.equal(p.PasswordReusePrevention, 5);
    assert.equal(p.HardExpiry, true);
    assert.equal(passwordPolicyError(from), null);
    assert.match(passwordPolicyError({ ...from, min: 5 })!, /6–128/);
    assert.match(passwordPolicyError({ ...from, maxAge: 2000 })!, /1095/);
    assert.match(passwordPolicyError({ ...from, reuse: 25 })!, /24/);
  });
});

describe("MFA", () => {
  it("the seed is base64 of the base32 text", () => {
    assert.equal(decodeBase64Text(b64("héllo")), "héllo");
    assert.equal(mfaSeed(b64("JBSWY3DPEHPK3PXPJBSWY3DP")), "JBSW Y3DP EHPK 3PXP JBSW Y3DP");
  });

  it("QR data URL", () => {
    assert.equal(qrDataUrl("iVBOR\nw0K"), "data:image/png;base64,iVBORw0K");
  });

  it("codes", () => {
    assert.equal(mfaCodesError("123456", "654321"), null);
    assert.match(mfaCodesError("12345", "654321")!, /6-digit/);
    assert.match(mfaCodesError("123456", "123456")!, /consecutive/);
    assert.equal(cleanCode(" 123 456 7"), "123456");
  });
});

describe("credential report", () => {
  it("parseCsv handles quotes, doubled quotes and CRLF", () => {
    assert.deepEqual(parseCsv('a,b\r\n"x, y","say ""hi"""\n1,\n'), [
      ["a", "b"],
      ["x, y", 'say "hi"'],
      ["1", ""],
    ]);
    assert.deepEqual(parseCsv("a,b"), [["a", "b"]]);
    assert.deepEqual(parseCsv(""), []);
  });

  it("credentialReport keys rows by column", () => {
    const csv = "user,arn,password_enabled,mfa_active\n<root_account>,arn:aws:iam::1:root,not_supported,false\nalice,arn:aws:iam::1:user/alice,true,true\n";
    const r = credentialReport(b64(csv));
    assert.deepEqual(r.columns, ["user", "arn", "password_enabled", "mfa_active"]);
    assert.equal(r.rows.length, 2);
    assert.equal(r.rows[1].user, "alice");
    assert.equal(r.rows[1].mfa_active, "true");
    assert.equal(reportCell(r.rows[0].password_enabled), "—");
    assert.equal(reportCell("N/A"), "—");
    assert.equal(reportCell(undefined), "—");
    assert.equal(reportCell("2026-10-22T00:00:00+00:00"), "2026-10-22T00:00:00+00:00");
  });
});

describe("policy simulator", () => {
  const base: SimulationDraft = {
    mode: "principal",
    sourceArn: "arn:aws:iam::123456789012:user/alice",
    policies: [],
    boundaries: [],
    actions: "s3:GetObject, s3:PutObject\ns3:ListBucket",
    resources: "",
    context: [],
    resourcePolicy: "",
    callerArn: "",
  };

  it("splitList", () => {
    assert.deepEqual(splitList(" a, b\n c  d,,"), ["a", "b", "c", "d"]);
  });

  it("principal mode", () => {
    assert.equal(simulationError(base), null);
    assert.deepEqual(simulationRequest(base), {
      action: "SimulatePrincipalPolicy",
      params: {
        PolicySourceArn: "arn:aws:iam::123456789012:user/alice",
        PolicyInputList: [],
        ActionNames: ["s3:GetObject", "s3:PutObject", "s3:ListBucket"],
        ResourceArns: [],
        ContextEntries: [],
        ResourcePolicy: undefined,
        CallerArn: undefined,
        PermissionsBoundaryPolicyInputList: [],
      },
    });
  });

  it("custom mode with context, resource policy and boundary (documents sent as typed)", () => {
    const doc = '{\n  "Statement": []\n}';
    const d: SimulationDraft = {
      ...base,
      mode: "custom",
      sourceArn: "",
      policies: [doc, "  "],
      boundaries: [doc],
      resources: "arn:aws:s3:::b/*",
      context: [
        { key: "aws:SourceIp", type: "ip", values: " 10.0.0.1 " },
        { key: "s3:prefix", type: "stringList", values: "a/, b/" },
        { key: "", type: "string", values: "" },
      ],
      resourcePolicy: doc,
      callerArn: "arn:aws:iam::123456789012:user/bob",
    };
    assert.equal(simulationError(d), null);
    const r = simulationRequest(d);
    assert.equal(r.action, "SimulateCustomPolicy");
    assert.deepEqual(r.params.PolicyInputList, [doc]);
    assert.equal(r.params.PolicySourceArn, undefined);
    assert.deepEqual(r.params.ContextEntries, [
      { ContextKeyName: "aws:SourceIp", ContextKeyType: "ip", ContextKeyValues: ["10.0.0.1"] },
      { ContextKeyName: "s3:prefix", ContextKeyType: "stringList", ContextKeyValues: ["a/", "b/"] },
    ]);
    assert.equal(r.params.ResourcePolicy, doc);
    assert.equal(r.params.CallerArn, "arn:aws:iam::123456789012:user/bob");
    assert.deepEqual(r.params.PermissionsBoundaryPolicyInputList, [doc]);
    assert.deepEqual(r.params.ResourceArns, ["arn:aws:s3:::b/*"]);
  });

  it("simulationError", () => {
    assert.match(simulationError({ ...base, sourceArn: "" })!, /Choose/);
    assert.match(simulationError({ ...base, mode: "custom", policies: [] })!, /Write a policy/);
    assert.match(simulationError({ ...base, mode: "custom", policies: ["{"] })!, /JSON/);
    assert.match(simulationError({ ...base, actions: " " })!, /action/);
    assert.match(simulationError({ ...base, resourcePolicy: "[]" })!, /Resource policy/);
    assert.match(simulationError({ ...base, context: [{ key: "", type: "string", values: "x" }] })!, /key/);
    assert.match(simulationError({ ...base, context: [{ key: "aws:SourceIp", type: "ip", values: "" }] })!, /value/);
  });

  it("contextEntries skips empty rows", () => {
    assert.deepEqual(contextEntries([{ key: " ", type: "string", values: " " }]), []);
  });

  it("decisions, positions and source documents", () => {
    assert.deepEqual(decisionLabel("allowed"), { label: "allowed", tone: "ok" });
    assert.equal(decisionLabel("explicitDeny").tone, "danger");
    assert.equal(decisionLabel("implicitDeny").tone, "muted");
    assert.equal(decisionLabel("other").label, "other");
    const m = { SourcePolicyId: "PolicyInputList.1", SourcePolicyType: "IAM Policy", StartPosition: { Line: "3", Column: "5" }, EndPosition: { Line: "9", Column: "6" } };
    assert.equal(statementRange(m), "3:5–9:6");
    assert.deepEqual(statementLines(m), [3, 9]);
    assert.equal(statementRange({ SourcePolicyId: "x" }), null);
    assert.equal(statementLines({ SourcePolicyId: "x", StartPosition: null }), null);
    const d: SimulationDraft = { ...base, mode: "custom", policies: ["", "P1", "P2"], boundaries: ["B1"], resourcePolicy: "R" };
    assert.equal(sourceDocument(m, d), "P1");
    assert.equal(sourceDocument({ ...m, SourcePolicyId: "PolicyInputList.2" }, d), "P2");
    assert.equal(sourceDocument({ ...m, SourcePolicyId: "PermissionsBoundaryPolicyInputList.1" }, d), "B1");
    assert.equal(sourceDocument({ SourcePolicyId: "ResourcePolicy", SourcePolicyType: "resource" }, d), "R");
    assert.equal(sourceDocument({ SourcePolicyId: "alice_inline" }, d), null);
  });

  it("simulate reads EvaluationResults as a list", async () => {
    reply = () => json({ EvaluationResults: { EvalActionName: "s3:GetObject", EvalDecision: "allowed", MatchedStatements: null, MissingContextValues: null }, IsTruncated: "false" });
    const r = await iamApi.simulate(simulationRequest(base));
    assert.equal(r.length, 1);
    assert.equal(r[0].EvalDecision, "allowed");
    assert.equal(calls[0].body.Action, "SimulatePrincipalPolicy");
    assert.deepEqual(calls[0].body.ActionNames, ["s3:GetObject", "s3:PutObject", "s3:ListBucket"]);
    assert.equal(calls[0].body.ResourceArns, undefined);
  });
});
