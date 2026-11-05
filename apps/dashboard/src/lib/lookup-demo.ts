/**
 * The demo owner's lookups (`memoryLookup`): what the pickers show in `?demo`. Linear matches the
 * demo's connected Acme workspace (`DEMO_TEAMS` in agents-demo.ts), Slack its "Acme" workspace
 * (`demoIntegrations`: installed by Ada, Grace a member). Discord isn't set up in the demo, so its
 * lookups say so there; its rows are for tests.
 */

import { memoryLookup, type LookupItem, type LookupRows } from "./lookup-api";

const TEAMS: LookupItem[] = [
  { id: "team_eng", key: "ENG", label: "Engineering", hint: "ENG", color: "#5e6ad2" },
  { id: "team_web", key: "WEB", label: "Web", hint: "WEB", color: "#26b5ce" },
  { id: "team_ops", key: "OPS", label: "Operations", hint: "OPS", color: "#f2c94c" },
];

const STATES: [string, string, string][] = [
  ["triage", "Triage", "#fc7840"],
  ["backlog", "Backlog", "#bec2c8"],
  ["unstarted", "Todo", "#e2e2e2"],
  ["started", "In Progress", "#f2c94c"],
  ["started", "In Review", "#0f783c"],
  ["completed", "Done", "#5e6ad2"],
  ["canceled", "Canceled", "#95a2b3"],
];

const statesOf = (t: LookupItem): LookupItem[] =>
  STATES.filter(([type]) => type !== "triage" || t.key === "ENG").map(([type, name, color]) => ({
    id: `st_${t.key!.toLowerCase()}_${name.toLowerCase().replace(/ /g, "_")}`,
    label: name,
    icon: type,
    color,
    group: t.label,
    hint: t.key,
  }));

const USERS: LookupItem[] = [
  { id: "usr_ada", label: "Ada Lovelace", key: "[EMAIL]", hint: "[EMAIL]", icon: "user" },
  { id: "usr_grace", label: "Grace Hopper", key: "[EMAIL]", hint: "[EMAIL]", icon: "user" },
  { id: "usr_alan", label: "Alan Turing", key: "[EMAIL]", hint: "[EMAIL]", icon: "user" },
  { id: "usr_linus", label: "Linus Torvalds", key: "[EMAIL]", hint: "[EMAIL]", icon: "user" },
  { id: "usr_roll", label: "roll", key: "[EMAIL]", hint: "Agent", icon: "agent" },
];

const LABELS: LookupItem[] = [
  { id: "lab_bug", label: "Bug", key: "Bug", color: "#eb5757", hint: "Workspace" },
  { id: "lab_feature", label: "Feature", key: "Feature", color: "#bb87fc", hint: "Workspace" },
  { id: "lab_improvement", label: "Improvement", key: "Improvement", color: "#4ea7fc", hint: "Workspace" },
  { id: "lab_roll", label: "roll", key: "roll", color: "#4c8dff", hint: "Workspace" },
  { id: "lab_security", label: "Security", key: "Security", color: "#f2994a", hint: "Workspace" },
];
const TEAM_LABELS: Record<string, LookupItem[]> = {
  team_eng: [
    { id: "lab_eng_backend", label: "Area / Backend", key: "Backend", color: "#26b5ce", hint: "ENG" },
    { id: "lab_eng_infra", label: "Area / Infra", key: "Infra", color: "#0f783c", hint: "ENG" },
  ],
  team_web: [{ id: "lab_web_design", label: "Design", key: "Design", color: "#f7c8c1", hint: "WEB" }],
};

const PROJECTS: LookupItem[] = [
  { id: "prj_auth", label: "Auth revamp", color: "#5e6ad2", hint: "started" },
  { id: "prj_billing", label: "Billing v2", color: "#26b5ce", hint: "planned" },
  { id: "prj_mobile", label: "Mobile app", color: "#bb87fc", hint: "backlog" },
];

const ISSUES: LookupItem[] = [
  { id: "iss_eng_12", key: "ENG-12", hint: "ENG-12", label: "Login fails with SSO", icon: "unstarted", color: "#e2e2e2" },
  { id: "iss_eng_14", key: "ENG-14", hint: "ENG-14", label: "Flaky checkout test on CI", icon: "started", color: "#f2c94c" },
  { id: "iss_eng_15", key: "ENG-15", hint: "ENG-15", label: "Rate limit the export endpoint", icon: "backlog", color: "#bec2c8" },
  { id: "iss_web_7", key: "WEB-7", hint: "WEB-7", label: "Dark mode for settings", icon: "started", color: "#0f783c" },
  { id: "iss_ops_3", key: "OPS-3", hint: "OPS-3", label: "Rotate the staging certificates", icon: "completed", color: "#5e6ad2" },
];

// ── Slack: the "Acme" workspace ──

const SLACK_WORKSPACES: LookupItem[] = [{ id: "T04ACME", key: "chi_demo1", label: "Acme", hint: "T04ACME", icon: "team" }];

const SLACK_CHANNELS: LookupItem[] = [
  { id: "C01GENERAL", key: "general", label: "general", hint: "48 members", icon: "channel" },
  { id: "C02DEPLOYS", key: "deploys", label: "deploys", hint: "12 members", icon: "channel" },
  { id: "C03INCIDENTS", key: "incidents", label: "incidents", hint: "21 members", icon: "channel" },
  { id: "G04ONCALL", key: "oncall", label: "oncall", hint: "5 members", icon: "private" },
  { id: "C05RANDOM", key: "random", label: "random", hint: "not a member", icon: "channel" },
];

const SLACK_USERS: LookupItem[] = [
  { id: "U02ADA", key: "ada", label: "Ada Lovelace", hint: "@ada", icon: "user" },
  { id: "U03GRACE", key: "grace", label: "Grace Hopper", hint: "@grace", icon: "user" },
  { id: "U04ALAN", key: "alan", label: "Alan Turing", hint: "@alan", icon: "user" },
  { id: "U05LINUS", key: "linus", label: "Linus Torvalds", hint: "@linus", icon: "user" },
  { id: "U06MARGARET", key: "margaret", label: "Margaret Hamilton", hint: "@margaret", icon: "user" },
];

const SLACK_GROUPS: LookupItem[] = [
  { id: "S01ONCALL", key: "oncall", label: "On call", hint: "@oncall", icon: "group" },
  { id: "S02WEB", key: "web-team", label: "Web team", hint: "@web-team", icon: "group" },
];

// ── Discord: a server for tests ──

const DISCORD_CHANNELS: LookupItem[] = [
  { id: "1100000000000000010", key: "welcome", label: "welcome", icon: "channel" },
  { id: "1100000000000000011", key: "deploys", label: "deploys", group: "Work", icon: "channel" },
  { id: "1100000000000000012", key: "ideas", label: "ideas", hint: "forum", group: "Work", icon: "channel" },
];

const DISCORD_ROLES: LookupItem[] = [
  { id: "1100000000000000021", key: "Maintainers", label: "Maintainers", color: "#4c8dff", icon: "role" },
  { id: "1100000000000000022", key: "Triage", label: "Triage", color: "#2ecc71", icon: "role" },
];

const DISCORD_MEMBERS: LookupItem[] = [
  { id: "1100000000000000042", key: "ada", label: "Ada", hint: "@ada", icon: "user" },
  { id: "1100000000000000043", key: "grace", label: "Grace", hint: "@grace", icon: "user" },
];

export const DEMO_LOOKUPS: LookupRows = {
  "slack/workspaces": SLACK_WORKSPACES,
  "slack/channels": SLACK_CHANNELS,
  "slack/users": SLACK_USERS,
  "slack/usergroups": SLACK_GROUPS,
  "slack/targets": [...SLACK_CHANNELS.map((c) => ({ ...c, group: "Channels" })), ...SLACK_USERS.map((u) => ({ ...u, group: "People" }))],
  "discord/guilds": [{ id: "1100000000000000001", key: "chi_demo2", label: "Acme Discord", hint: "1100000000000000001", icon: "team" }],
  "discord/channels": DISCORD_CHANNELS,
  "discord/roles": DISCORD_ROLES,
  "discord/members": DISCORD_MEMBERS,
  "linear/teams": TEAMS,
  "linear/states": (team) => (team ? TEAMS.filter((t) => t.id === team) : TEAMS).flatMap(statesOf),
  "linear/users": USERS,
  "linear/labels": (team) => [...LABELS, ...(team ? (TEAM_LABELS[team] ?? []) : Object.values(TEAM_LABELS).flat())],
  "linear/projects": PROJECTS,
  "linear/cycles": (team) =>
    team
      ? [
          { id: `cyc_${team}_41`, key: "41", label: "Cycle 41", hint: "Current · Nov 3 – Nov 16" },
          { id: `cyc_${team}_42`, key: "42", label: "Cycle 42", hint: "Nov 17 – Nov 30" },
        ]
      : [],
  "linear/issues": ISSUES,
  // roll code's own (the demo's repos in agents-demo.ts, its agent tokens).
  "code/repos": [
    { id: "roll", label: "roll", key: "acme/roll", hint: "keep the code rolling", icon: "repo" },
    { id: "web", label: "web", key: "acme/web", hint: "The marketing site", icon: "repo" },
    { id: "infra", label: "infra", key: "acme/infra", icon: "repo" },
    { id: "docs", label: "docs", key: "acme/docs", hint: "Guides and the API reference", icon: "repo" },
  ],
  "code/agents": [
    { id: "triage", label: "triage", key: "agent:triage", icon: "agent" },
    { id: "release", label: "release", key: "agent:release", icon: "agent" },
  ],
};

export const demoLookup = () => memoryLookup(DEMO_LOOKUPS, { discord: { code: "not_connected", message: "Discord isn't set up on this server" } });
