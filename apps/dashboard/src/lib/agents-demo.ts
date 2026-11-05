/**
 * A simulated owner for `/dashboard/agents?demo`: two machines, projects, threads in every state,
 * subagents, approvals that arrive and wait, and text that streams, all behind the same
 * `AgentsTransport` the relay uses, so the canvas can't tell the difference. For development,
 * screenshots and the marketing page; never talks to the network.
 */

import { avatarSvg } from "@roll/brand/geometry";
import { checkProjectIcon } from "./agents-identity";
import type {
  PullOutcome,
  PullResult,
  Approval,
  ChangeStatus,
  Landing,
  Checkpoint,
  Command,
  ContextUsage,
  DiffStat,
  Entity,
  FileChange,
  FsEntry,
  Machine,
  MachineId,
  MachineInfo,
  MachineSettings,
  Mirrored,
  Node,
  NodeBody,
  Project,
  ModelOption,
  ProviderInfo,
  Question,
  Reply,
  Request,
  RequestResponse,
  Run,
  SandboxInfo,
  State,
  StreamEvent,
  Terminal,
  Thread,
  ThreadId,
  Todo,
} from "./agents-api";
import { DEFAULT_AUTO_SETTLE, EMPTY_INBOX, NO_SETTINGS, reachable } from "./agents-api";
import type { AgentsTransport } from "./agents-client";
import { DemoAutomations } from "./automations-demo";
import { memoryCanvas, memorySettings, type AgentsServices, type RepoChoice } from "./agents-services";
import { offeredProviders, settingsError } from "./machine-settings";
import { PLAN_IMPLEMENTATION_PREFIX, planTitle } from "./agents-plan";
import { ApiError } from "./code-api";
import { memoryLinear, type LinearStatus, type LinearTeam } from "./linear-api";
import type { Sandbox, SandboxSize } from "./sandboxes-api";
import { isMemberId, MAX_MEMBERS, type ChatInstall, type ChatPlatform, type Integrations, type IntegrationsApi } from "./integrations-api";
import { reasoningLevels } from "@/lib/agents-model-options";

// ── the cast ──

/** Codex reports this window for its GPT-5.5 models (`modelContextWindow`). */
const CODEX_WINDOW = 258_400;

/** The demo models' windows, as the daemon's adapters report them (T3's model manifest for Claude). */
function contextWindow(model: string | null): number | null {
  if (!model) return null;
  if (model.startsWith("gpt-")) return CODEX_WINDOW;
  if (model.startsWith("claude-")) return /-4-5\b/.test(model) ? 200_000 : 1_000_000;
  return null;
}

/** The demo's Claude options, as the daemon's catalog describes them (T3's model manifest). */
const CLAUDE_EFFORTS = ["low", "medium", "high", "xhigh", "max"];
const ultra = [
  { id: "ultracode", label: "Ultracode", description: "xhigh effort plus multi-agent workflow orchestration" },
  { id: "ultrathink", label: "Ultrathink", description: "Starts each prompt with Ultrathink" },
];
const claudeReasoning = (def: string, extra = ultra): ModelOption => ({ ...reasoningLevels(CLAUDE_EFFORTS, def), choices: [...reasoningLevels(CLAUDE_EFFORTS).choices, ...extra] });
const fastMode: ModelOption = { type: "boolean", id: "fastMode", label: "Fast Mode", description: "Faster output at a higher price", default: false, role: "speed" };
const contextWindowOption = (def: string): ModelOption => ({
  type: "select",
  id: "contextWindow",
  label: "Context Window",
  choices: [
    { id: "200k", label: "200k" },
    { id: "1m", label: "1M" },
  ],
  default: def,
  role: "context_window",
});

const claude = (auth: ProviderInfo["auth"] = "ready"): ProviderInfo => ({
  id: "claude",
  name: "Claude Code",
  installed: true,
  version: "2.1.290",
  auth,
  detail: auth === "ready" ? "Logged in (Max)" : "Run `claude /login`",
  models: [
    { id: "claude-opus-5-5", name: "Claude Opus 5.5", short_name: "Opus 5.5", default: true, options: [claudeReasoning("medium"), fastMode] },
    { id: "claude-sonnet-5", name: "Claude Sonnet 5", short_name: "Sonnet 5", default: false, options: [claudeReasoning("high", ultra.slice(1))] },
    { id: "claude-haiku-5", name: "Claude Haiku 5", short_name: "Haiku 5", default: false, options: [claudeReasoning("medium")] },
    { id: "claude-opus-4-6", name: "Claude Opus 4.6", short_name: "Opus 4.6", default: false, legacy: true, options: [{ ...reasoningLevels(["low", "medium", "high", "max"], "high") }, fastMode, contextWindowOption("1m")] },
    { id: "claude-sonnet-4-6", name: "Claude Sonnet 4.6", short_name: "Sonnet 4.6", default: false, legacy: true, options: [reasoningLevels(["low", "medium", "high", "max"], "high"), contextWindowOption("200k")] },
    { id: "claude-haiku-4-5", name: "Claude Haiku 4.5", short_name: "Haiku 4.5", default: false, legacy: true, options: [{ type: "boolean", id: "thinking", label: "Thinking", default: true }] },
  ],
  update_required: [{ id: "claude-haiku-5-5", name: "Claude Haiku 5.5", min_version: "2.1.293" }],
  capabilities: { steer: true, fork: true, rollback: false, plan_mode: true, images: true, subagents: true, mode_switch: true, compact: true },
});

/** Codex's service tiers, as `model/list` reports them. */
const serviceTier = (def = "default"): ModelOption => ({
  type: "select",
  id: "serviceTier",
  label: "Service Tier",
  choices: [
    { id: "default", label: "Standard" },
    { id: "priority", label: "Fast", description: "2x speed, increased usage" },
  ],
  default: def,
  role: "speed",
});

const codex = (auth: ProviderInfo["auth"] = "ready"): ProviderInfo => ({
  id: "codex",
  name: "Codex",
  installed: true,
  version: "0.164.0",
  auth,
  detail: auth === "ready" ? "Logged in with ChatGPT" : "Run `codex login`",
  models: [
    { id: "gpt-5.5-codex", name: "GPT-5.5 Codex", default: true, options: [reasoningLevels(["low", "medium", "high", "xhigh"], "medium"), serviceTier()] },
    { id: "gpt-5.5", name: "GPT-5.5", default: false, options: [reasoningLevels(["minimal", "low", "medium", "high"], "medium"), serviceTier("priority")] },
    { id: "gpt-5.5-mini", name: "GPT-5.5 Mini", default: false, options: [reasoningLevels(["low", "medium", "high"], "low")], images: false },
  ],
  update_required: [],
  capabilities: { steer: true, fork: true, rollback: true, plan_mode: true, images: true, subagents: true, mode_switch: false, compact: true },
});

const gemini: ProviderInfo = {
  id: "acp:gemini",
  name: "Gemini CLI",
  installed: true,
  version: "0.31.0",
  auth: "unknown",
  detail: "ACP agent from the daemon config",
  models: [
    { id: "gemini-3-pro", name: "Gemini 3 Pro", default: true, options: [] },
    { id: "gemini-3-flash", name: "Gemini 3 Flash", default: false, options: [] },
  ],
  update_required: [],
  capabilities: { steer: false, fork: false, rollback: false, plan_mode: false, images: true, subagents: false, mode_switch: false },
};

/** An ACP agent on the PATH that's still being checked, with nothing known yet. */
const grok: ProviderInfo = {
  id: "acp:grok",
  name: "Grok Build",
  installed: false,
  version: null,
  auth: "unknown",
  detail: "`grok` is not installed",
  models: [],
  update_required: [],
  capabilities: { steer: false, fork: false, rollback: false, plan_mode: true, images: true, subagents: false, mode_switch: true },
};

const DEMO_SIZES: SandboxSize[] = [
  { name: "standard", cpu: 2, memory_gb: 4, disk_gb: 20, hourly_cents: 3 },
  { name: "large", cpu: 4, memory_gb: 8, disk_gb: 40, hourly_cents: 6 },
  { name: "xl", cpu: 8, memory_gb: 16, disk_gb: 80, hourly_cents: 10 },
];

/** How long a demo sandbox with nothing running stays up before it sleeps (scaled by `speed`). */
const DEMO_IDLE_MS = 20_000;

const DEMO_REPOS: RepoChoice[] = [
  { owner: "acme", name: "roll", full_name: "acme/roll", description: "keep the code rolling", visibility: "public", updated_at: 1_762_000_000 },
  { owner: "acme", name: "web", full_name: "acme/web", description: "The marketing site", visibility: "private", updated_at: 1_761_900_000 },
  { owner: "acme", name: "infra", full_name: "acme/infra", description: null, visibility: "private", updated_at: 1_761_800_000 },
  { owner: "acme", name: "docs", full_name: "acme/docs", description: "Guides and the API reference", visibility: "public", updated_at: 1_761_700_000 },
];

/** The demo owner's Linear workspace: roll installed as its agent, deliveries coming in. */
function demoLinear(at = Date.now()): LinearStatus {
  const min = 60_000;
  return {
    connected: true,
    app: { oauth: true, webhook: true, webhook_url: "https://roll.example/v1/code/linear/webhook", redirect_uri: "https://roll.example/v1/code/linear/oauth/callback", secret_key: true },
    connection: {
      kind: "oauth",
      workspace: { id: "org_acme", name: "Acme", url_key: "acme", url: "https://linear.app/acme" },
      user: { id: "usr_roll", name: "roll" },
      agent: true,
      scopes: ["read", "write", "issues:create", "comments:create", "app:assignable", "app:mentionable"],
      status: "ok",
      error: null,
      expires_at: null,
      created_at: at - 9 * 24 * 60 * min,
      created_by: "demo",
      settings: { repos: { ENG: "roll", WEB: "web" }, default_repo: "roll" },
      webhook: { mode: "app", url: "https://roll.example/v1/code/linear/webhook", status: "ok", secret_hint: null, last_delivery_at: at - 2 * min, rejected_at: null, rejected_why: null },
    },
    deliveries: [
      { id: "ldl_demo4", at: at - 2 * min, delivery: "dlv_4", type: "AgentSessionEvent", action: "created", summary: "ENG-142 Speed up the lander's conflict check: Ada assigned roll", result: "recorded", events: 1 },
      { id: "ldl_demo3", at: at - 14 * min, delivery: "dlv_3", type: "Comment", action: "create", summary: "ENG-139: Grace commented", result: "recorded", events: 1 },
      { id: "ldl_demo2", at: at - 15 * min, delivery: "dlv_2", type: "Comment", action: "create", summary: "ENG-139: Grace commented", result: "duplicate", events: 0 },
      { id: "ldl_demo1", at: at - 52 * min, delivery: "dlv_1", type: "Issue", action: "update", summary: "WEB-31 Pricing page copy: moved to In Progress", result: "ignored", events: 0 },
    ],
  };
}

const DEMO_TEAMS: LinearTeam[] = [
  { id: "team_eng", key: "ENG", name: "Engineering" },
  { id: "team_web", key: "WEB", name: "Web" },
  { id: "team_ops", key: "OPS", name: "Operations" },
];

/**
 * The demo's chat ops, in memory: Slack connected to "Acme" (its members editable, disconnecting
 * forgets it), Discord not configured on the server, the GitHub App configured with two repos.
 * Connecting can't leave the demo, so it says why instead.
 */
export function demoIntegrations(): IntegrationsApi {
  const state: Integrations = {
    slack: {
      configured: true,
      installs: [
        {
          id: "chi_demo1",
          team_id: "T04ACME",
          team_name: "Acme",
          bot_user_id: "U07ROLLBOT",
          installer: "U02ADA",
          members: ["U03GRACE"],
          anyone: false,
          created_at: 1_761_500_000_000,
        },
      ],
    },
    discord: { configured: false, installs: [] },
    github: {
      configured: true,
      app_slug: "roll-code",
      install_url: "https://github.com/apps/roll-code/installations/new",
      repos: [
        { repo: "acme/roll", github: "acme-inc/roll", url: "https://github.com/acme-inc/roll", connected: true, mirror: "ok" },
        { repo: "acme/web", github: "acme-inc/web", url: "https://github.com/acme-inc/web", connected: true, mirror: "pending" },
      ],
    },
    sealing: true,
  };
  const find = (platform: ChatPlatform, id: string) => {
    const i = state[platform].installs.find((x) => x.id === id);
    if (!i) throw new ApiError(`integration ${id} not found`, 404, null);
    return i;
  };
  return {
    list: async () => structuredClone(state),
    install: async (_o, platform) => {
      throw new ApiError(`The demo can't connect ${platform === "slack" ? "Slack" : "Discord"}. On your own server, this opens its install page.`, 422, null);
    },
    update: async (_o, platform, id, patch) => {
      const i = find(platform, id);
      const members = patch.members?.map((m) => m.trim()).filter(Boolean);
      if (members && (members.length > MAX_MEMBERS || !members.every(isMemberId))) throw new ApiError("members are at most 200 platform user ids (letters and digits)", 400, null);
      const next: ChatInstall = { ...i, members: members ? [...new Set(members)] : i.members, anyone: patch.anyone ?? i.anyone };
      state[platform].installs = state[platform].installs.map((x) => (x.id === id ? next : x));
      return structuredClone(next);
    },
    remove: async (_o, platform, id) => {
      find(platform, id);
      state[platform].installs = state[platform].installs.filter((x) => x.id !== id);
    },
  };
}

/**
 * What a demo daemon reports about its providers and settings, as `roll agent up` does: the
 * providers that are on (custom models added), and the settings with what they fall back to.
 * `fileEnv` is what its `config.toml` sets, by name.
 */
function demoConfig(home: string, all: readonly ProviderInfo[], set: MachineSettings, fileEnv: Record<string, string[]> = {}): Pick<MachineInfo, "providers" | "settings"> {
  const data = `${home}/.local/share/roll/agent`;
  return {
    providers: offeredProviders(all, set),
    settings: {
      set,
      base: { worktree_dir: `${data}/worktrees`, branch_prefix: "roll", clone_dir: `${home}/roll`, pull_every: 60, max_depth: 3, max_children: 6, git_name: home.startsWith("/Users/") ? "Ada Lovelace" : null, git_email: null },
      providers: all.map((p) => ({ id: p.id, name: p.name, binary: p.id.startsWith("acp:") ? p.id.slice(4) : null, args: p.id.startsWith("acp:") ? 1 : 0, env: fileEnv[p.id] ?? [] })),
      config_path: `${data}/config.toml`,
    },
  };
}

const MBP = "mch_demo_ada";
const BOX = "mch_demo_box";

const DIFF_QUEUE = `@@ -88,13 +88,17 @@ impl Queue {
     pub async fn claim(&self, repo: &RepoId) -> Result<Option<Claim>> {
-        let lease = self.leases.get(repo);
-        if lease.is_some_and(|l| l.expires_at > now()) {
-            return Ok(None);
-        }
-        self.leases.insert(repo.clone(), Lease::new(now() + LEASE));
+        // One write closure: read and claim together, so two nodes can't both win.
+        self.db
+            .write(|tx| {
+                let held = tx.lease(repo)?;
+                if held.is_some_and(|l| l.expires_at > tx.now()) {
+                    return Ok(None);
+                }
+                tx.put_lease(repo, Lease::new(tx.now() + LEASE))?;
+                Ok(Some(Claim::new(repo)))
+            })
+            .await
-        Ok(Some(Claim::new(repo)))
     }
`;

const DIFF_TEST = `@@ -210,6 +210,22 @@ async fn two_nodes_claim_once() {
+#[tokio::test]
+async fn claim_is_atomic_under_contention() {
+    let (a, b) = two_nodes().await;
+    let repo = repo("acme/web");
+    let (x, y) = tokio::join!(a.claim(&repo), b.claim(&repo));
+    assert_eq!(x.unwrap().is_some() as u8 + y.unwrap().is_some() as u8, 1);
+}
`;

const DIFF_STATUS = `@@ -40,6 +40,12 @@ pub struct StatusArgs {
     /// Only this repo.
     pub repo: Option<String>,
+    /// Print the status as JSON.
+    #[arg(long)]
+    pub json: bool,
 }
@@ -71,4 +77,8 @@ pub async fn status(args: StatusArgs) -> Result<()> {
-    print_table(&status);
+    if args.json {
+        println!("{}", serde_json::to_string_pretty(&status)?);
+    } else {
+        print_table(&status);
+    }
`;

const DIFF_NEXT = `@@ -1,6 +1,6 @@
 {
   "dependencies": {
-    "next": "16.3.2",
+    "next": "16.4.1",
     "react": "19.3.0",
`;

const fc = (path: string, change: FileChange["change"], diff: string): FileChange => ({ path, change, diff });

function stat(files: FileChange[]): DiffStat {
  let additions = 0;
  let deletions = 0;
  for (const f of files) {
    for (const l of (f.diff ?? "").split("\n")) {
      if (l.startsWith("+")) additions++;
      else if (l.startsWith("-")) deletions++;
    }
  }
  return { files: files.length, additions, deletions };
}

// ── scripts: what each simulated run does ──

type Step =
  | { say: string }
  | { think: string }
  | { plan: string }
  | { todos: Todo[] }
  | { tool: { name: string; kind: "command" | "edit" | "read" | "search" | "web" | "mcp" | "other"; title: string; input?: unknown; output?: string; files?: FileChange[]; exit?: number; ms?: number } }
  | { sub: { agent: string; prompt: string; steps: Step[]; result: string; model?: string } }
  | { ask: Pick<Approval, "action" | "title" | "decisions"> & Partial<Approval> }
  | { questions: Question[] }
  | { wait: number }
  /** The provider compacts the context (T3's compaction row): to `after` tokens, a quarter of it by default. */
  | { compact: { after?: number } }
  /** Hand tasks to subagents through roll's orchestrator MCP: each is a thread of its own under this one. */
  | { delegate: DemoTask[] };

/** A task an orchestrator delegates (`delegate_task`): a child thread that runs `steps`. */
interface DemoTask {
  title: string;
  task: string;
  provider: string;
  model: string;
  steps: Step[];
}

/** One line of a goal, for titles and briefs. */
function gist(goal: string, max = 60) {
  const line = goal.trim().split("\n")[0].replace(/[.!?]+$/, "");
  return line.length > max ? `${line.slice(0, max - 1)}…` : line;
}

/**
 * What an orchestrator does with a goal: look at the project, plan, tell the user, hand three
 * parts to subagents in their own worktrees (on the machine's other provider where it has one),
 * and end its turn; each result wakes it (`reviewScript`).
 */
function orchestratorScript(goal: string, providers: readonly string[]): Step[] {
  const g = gist(goal);
  const other = providers.includes("codex") ? ["codex", "gpt-5.5-codex"] : ["claude", "claude-sonnet-5"];
  const tasks: DemoTask[] = [
    {
      title: `Build: ${gist(goal, 40)}`,
      task: `Implement this in the project: ${g}.\n\nFollow the patterns around it and keep the change small. Done when it builds and the existing tests pass; report the files you changed.`,
      provider: "claude",
      model: "claude-opus-5-5",
      steps: [
        { think: "Find where this belongs before touching anything." },
        { tool: { name: "Grep", kind: "search", title: "settings", output: "src/app/settings/page.tsx\nsrc/components/settings-form.tsx\n", ms: 900 } },
        { tool: { name: "Read", kind: "read", title: "src/components/settings-form.tsx", ms: 800 } },
        { tool: { name: "Edit", kind: "edit", title: "src/components/settings-form.tsx", files: [fc("src/components/settings-form.tsx", "modify", "@@ -12,3 +12,9 @@\n   return (\n     <form>\n+      <ThemeSwitch />\n")], ms: 1100 } },
        { tool: { name: "Bash", kind: "command", title: "pnpm build", output: "▲ Next.js 16.4.1\n✓ Compiled successfully in 9.8s\n", exit: 0, ms: 1800 } },
        { say: `Done: ${g}. Changed \`src/components/settings-form.tsx\`; the build passes and the existing tests are green.` },
      ],
    },
    {
      title: `Tests: ${gist(goal, 40)}`,
      task: `Write tests for: ${g}.\n\nCover the main path and one edge case, next to the existing tests. Done when they pass.`,
      provider: other[0],
      model: other[1],
      steps: [
        { tool: { name: "shell", kind: "read", title: "ls src/**/*.test.tsx", output: "src/components/settings-form.test.tsx\n", exit: 0, ms: 700 } },
        { tool: { name: "apply_patch", kind: "edit", title: "src/components/settings-form.test.tsx", files: [fc("src/components/settings-form.test.tsx", "modify", "@@ -40,0 +41,8 @@\n+it(\"remembers the choice\", async () => {\n+  render(<SettingsForm />);\n+});\n")], ms: 1000 } },
        {
          ask: {
            action: "command",
            title: "Run `pnpm test settings`",
            command: "pnpm test settings",
            reason: "Run the new tests",
            tool: "shell",
            input: { command: "pnpm test settings" },
            decisions: ["accept", "accept_for_session", "decline", "cancel"],
          },
        },
        { tool: { name: "shell", kind: "command", title: "pnpm test settings", output: " ✓ src/components/settings-form.test.tsx (6 tests) 412ms\n\n Test Files  1 passed (1)\n      Tests  6 passed (6)\n", exit: 0 } },
        { say: "Added two tests in `src/components/settings-form.test.tsx` (the main path and a reload); all 6 pass." },
      ],
    },
    {
      title: `Docs: ${gist(goal, 40)}`,
      task: `Document this for users: ${g}.\n\nOne short section in the docs where the feature lives, and a line in the changelog.`,
      provider: "claude",
      model: "claude-sonnet-5",
      steps: [
        { tool: { name: "Read", kind: "read", title: "docs/settings.md", ms: 700 } },
        { tool: { name: "Edit", kind: "edit", title: "docs/settings.md", files: [fc("docs/settings.md", "modify", "@@ -20,0 +21,4 @@\n+## Appearance\n+\n+Pick light, dark or the system's theme.\n")], ms: 900 } },
        { tool: { name: "Edit", kind: "edit", title: "CHANGELOG.md", files: [fc("CHANGELOG.md", "modify", "@@ -1,0 +2,1 @@\n+- Settings: choose the theme.\n")], ms: 600 } },
        { say: "Documented it in `docs/settings.md` (Appearance) and added a changelog line." },
      ],
    },
  ];
  return [
    { think: `The goal: "${g}". Look at how the project is laid out before splitting it up.` },
    { tool: { name: "Bash", kind: "command", title: "git ls-files | head -40", output: "README.md\nsrc/app/settings/page.tsx\nsrc/components/settings-form.tsx\nsrc/components/settings-form.test.tsx\ndocs/settings.md\n", exit: 0, ms: 900 } },
    { tool: { name: "Read", kind: "read", title: "README.md", ms: 700 } },
    { plan: `1. Build it (Claude Code, own worktree)\n2. Tests for it (${other[0] === "codex" ? "Codex" : "Claude Code"}, own worktree)\n3. Docs and changelog (Claude Code, own worktree)\n4. Check each result, then sum up` },
    { say: "The plan: three agents in parallel, each in its own worktree so they never collide. I'll check each result as it comes in." },
    { tool: { name: "orchestrator_capabilities", kind: "mcp", title: "roll · orchestrator_capabilities", ms: 500 } },
    { delegate: tasks },
  ];
}

/** An orchestrator woken by results: check them, and sum up once nothing is left. */
function reviewScript(titles: string[], left: number): Step[] {
  const done = titles.join(", ");
  return left > 0
    ? [
        { think: `${done} finished. Look at the diff before trusting it.` },
        { tool: { name: "Bash", kind: "command", title: "git diff --stat main...", output: " 2 files changed, 14 insertions(+), 1 deletion(-)\n", exit: 0, ms: 900 } },
        { say: `${done}: done and checked. Waiting on ${left} more.` },
      ]
    : [
        { think: "Everything is back. Run the whole suite on the combined work." },
        { tool: { name: "Bash", kind: "command", title: "pnpm test", output: " Test Files  14 passed (14)\n      Tests  128 passed (128)\n", exit: 0, ms: 1600 } },
        { say: `All done. ${titles.length > 1 ? `${done} finished` : `${done} finished last`}; every part is in its own branch, the whole suite passes, and nothing is left open.` },
      ];
}

const flakyScript: Step[] = [
  { think: "The test claims a lease twice from two nodes. If the read and the write aren't in one transaction, both can see no lease and both claim it." },
  { say: "I'll start by reading the queue claim path and the test that flakes." },
  { tool: { name: "Read", kind: "read", title: "crates/code-control/src/queue.rs", input: { file_path: "crates/code-control/src/queue.rs" }, ms: 700 } },
  {
    todos: [
      { text: "Reproduce the flake", status: "in_progress" },
      { text: "Make claim() atomic", status: "pending" },
      { text: "Add a contention test", status: "pending" },
    ],
  },
  {
    sub: {
      agent: "Explore",
      prompt: "Find every caller of Queue::claim and how each handles None",
      steps: [
        { tool: { name: "Grep", kind: "search", title: "claim\\(", input: { pattern: "claim\\(" }, output: "src/queue.rs:88\nsrc/lander.rs:212\nsrc/cluster.rs:57\n", ms: 900 } },
        { tool: { name: "Read", kind: "read", title: "crates/code-control/src/lander.rs", ms: 800 } },
      ],
      result: "3 callers; all treat None as \"someone else holds it\" and retry after 250 ms.",
      model: "claude-haiku-5-5",
    },
  },
  { tool: { name: "Bash", kind: "command", title: "cargo test -p code-control claim -- --test-threads 16", input: { command: "cargo test -p code-control claim -- --test-threads 16" }, output: "running 4 tests\ntest queue::two_nodes_claim_once ... FAILED\n\nfailures:\n  left: 2\n right: 1\n\ntest result: FAILED. 3 passed; 1 failed\n", exit: 101, ms: 2200 } },
  { say: "Reproduced: with 16 threads both nodes win the claim about one run in ten. The read and the insert are two statements, so I'll put them in one `db.write` closure." },
  {
    tool: {
      name: "Edit",
      kind: "edit",
      title: "crates/code-control/src/queue.rs",
      files: [fc("crates/code-control/src/queue.rs", "modify", DIFF_QUEUE)],
      ms: 900,
    },
  },
  {
    tool: {
      name: "Edit",
      kind: "edit",
      title: "crates/code-control/tests/concurrency.rs",
      files: [fc("crates/code-control/tests/concurrency.rs", "modify", DIFF_TEST)],
      ms: 700,
    },
  },
  {
    todos: [
      { text: "Reproduce the flake", status: "completed" },
      { text: "Make claim() atomic", status: "completed" },
      { text: "Add a contention test", status: "in_progress" },
    ],
  },
  {
    ask: {
      action: "command",
      title: "Run `cargo test -p code-control`",
      command: "cargo test -p code-control",
      cwd: "/Users/ada/roll",
      reason: "Check the fix across the whole crate",
      tool: "Bash",
      input: { command: "cargo test -p code-control" },
      decisions: ["accept", "accept_for_session", "decline", "cancel"],
    },
  },
  { tool: { name: "Bash", kind: "command", title: "cargo test -p code-control", input: { command: "cargo test -p code-control" }, output: "running 412 tests\n...\ntest queue::claim_is_atomic_under_contention ... ok\ntest queue::two_nodes_claim_once ... ok\n\ntest result: ok. 412 passed; 0 failed; finished in 38.2s\n", exit: 0, ms: 2600 } },
  {
    todos: [
      { text: "Reproduce the flake", status: "completed" },
      { text: "Make claim() atomic", status: "completed" },
      { text: "Add a contention test", status: "completed" },
    ],
  },
  { say: "Fixed. `claim()` now reads and writes the lease in one `db.write` closure, so two nodes can't both win, and `claim_is_atomic_under_contention` races 16 claims to prove it. All 412 tests pass." },
];

const auditScript: Step[] = [
  { say: "Auditing every caller of `claim()` for the retry behaviour." },
  { tool: { name: "shell", kind: "command", title: "rg -n 'claim\\(' crates", output: "crates/code-control/src/queue.rs:88\ncrates/code-control/src/lander.rs:212\ncrates/code-control/src/cluster.rs:57\n", exit: 0, ms: 1100 } },
  { tool: { name: "read", kind: "read", title: "crates/code-control/src/cluster.rs", ms: 900 } },
  { say: "All three callers back off and retry; `cluster.rs` also checks `owns_repo` first, which is right." },
];

const ciScript: Step[] = [
  { think: "The repo runs GitHub Actions; roll checks read `roll.toml` from the default branch. A plan first, since this is plan mode." },
  { tool: { name: "shell", kind: "read", title: "cat .github/workflows/ci.yml", output: "name: ci\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n", exit: 0, ms: 800 } },
  {
    plan: "## Move CI to roll checks\n\n1. Add `roll.toml` with a `test` check running `pnpm test` in `node:22`.\n2. Make `test` a required check on `main`.\n3. Keep the GitHub workflow for one week, then delete it.\n\nNothing changes until you approve the plan.",
  },
  { ask: { action: "plan", title: "Carry out the plan", decisions: ["accept", "decline"] } },
  { tool: { name: "apply_patch", kind: "edit", title: "roll.toml", files: [fc("roll.toml", "add", '@@ -0,0 +1,4 @@\n+[checks.test]\n+image = "node:22"\n+run = "pnpm install && pnpm test"\n+required = true\n')], ms: 900 } },
  { say: "Added `roll.toml` with a required `test` check. The GitHub workflow stays for now." },
];

/** A plan a Codex run ended with in plan mode: no approval, the composer offers Implement (T3). */
const BILLING_PLAN =
  "# Export invoices as CSV\n\n1. Add `GET /v1/billing/invoices.csv` in `crates/billing/src/http.rs`, streaming rows from the `invoices` table (one owner, a date range).\n2. Reuse `InvoiceRow` for the columns: number, date, amount, currency, status.\n3. Add an **Export CSV** button beside the invoice list in `apps/dashboard/src/components/billing/invoices.tsx`.\n4. Test the route in `crates/billing/tests/http.rs` (empty range, one invoice, 10 000 invoices streamed).\n\nNothing changes until you say so.";

/** T3's "Implement": the plan comes back as a message in build mode, and the agent carries it out. */
function implementScript(plan: string): Step[] {
  const title = planTitle(plan) ?? "the plan";
  return [
    { think: `Carrying out "${title}" step by step.` },
    { todos: [{ text: "Add the route", status: "in_progress" }, { text: "Add the button", status: "pending" }, { text: "Test it", status: "pending" }] },
    { tool: { name: "apply_patch", kind: "edit", title: "crates/billing/src/http.rs", files: [fc("crates/billing/src/http.rs", "modify", "@@ -40,3 +40,9 @@\n     .route(\"/v1/billing/invoices\", get(list))\n+    .route(\"/v1/billing/invoices.csv\", get(export_csv))\n")], ms: 900 } },
    { todos: [{ text: "Add the route", status: "completed" }, { text: "Add the button", status: "in_progress" }, { text: "Test it", status: "pending" }] },
    { tool: { name: "apply_patch", kind: "edit", title: "apps/dashboard/src/components/billing/invoices.tsx", files: [fc("apps/dashboard/src/components/billing/invoices.tsx", "modify", "@@ -12,2 +12,3 @@\n       <h2>Invoices</h2>\n+      <a href={csvUrl}>Export CSV</a>\n")], ms: 800 } },
    { tool: { name: "shell", kind: "command", title: "cargo test -p billing invoices_csv", output: "running 3 tests\ntest invoices_csv_empty_range ... ok\ntest invoices_csv_one ... ok\ntest invoices_csv_streams_10k ... ok\n\ntest result: ok. 3 passed; 0 failed\n", exit: 0, ms: 1800 } },
    { todos: [{ text: "Add the route", status: "completed" }, { text: "Add the button", status: "completed" }, { text: "Test it", status: "completed" }] },
    { say: `Done: ${title.toLowerCase()}. The route streams rows, the button sits beside the invoice list, and three tests cover it.` },
  ];
}

/** In plan mode a follow-up refines the plan (T3's "Refine"): a new plan, still nothing changed. */
function refineScript(note: string): Step[] {
  return [
    { think: `Revising the plan: "${note.slice(0, 80)}".` },
    { plan: `${BILLING_PLAN.replace("\n\nNothing changes until you say so.", "")}\n5. ${note.trim().replace(/\.$/, "")}.\n\nNothing changes until you say so.` },
    { say: "Updated the plan." },
  ];
}

const nextScript: Step[] = [
  { say: "Upgrading Next.js. One decision first." },
  {
    questions: [
      {
        id: "scope",
        header: "Scope",
        question: "How far should the upgrade go?",
        options: [
          { label: "Patch only", description: "16.3.2 → 16.3.9, no code changes" },
          { label: "Minor", description: "16.4.1, adopt the new proxy API" },
        ],
        multi: false,
      },
      {
        id: "checks",
        header: "Checks",
        question: "What should I run afterwards?",
        options: [
          { label: "Typecheck", description: null },
          { label: "Unit tests", description: null },
          { label: "Playwright", description: null },
        ],
        multi: true,
      },
    ],
  },
  { tool: { name: "Bash", kind: "command", title: "pnpm up next@16.4.1", output: "Packages: +2 -2\nDone in 3.1s\n", exit: 0, ms: 1400 } },
  { tool: { name: "Edit", kind: "edit", title: "apps/web/package.json", files: [fc("apps/web/package.json", "modify", DIFF_NEXT)], ms: 600 } },
  { say: "Upgraded to Next 16.4.1; typecheck and unit tests pass." },
];

/** Work the demo starts by itself when it's quiet, so the canvas keeps rolling. */
const autoRuns: { prompt: string; steps: Step[] }[] = [
  {
    prompt: "Add a test for the empty-repo case in `roll code status`",
    steps: [
      { tool: { name: "Read", kind: "read", title: "crates/roll-cli/tests/code.rs", ms: 700 } },
      { tool: { name: "Edit", kind: "edit", title: "crates/roll-cli/tests/code.rs", files: [fc("crates/roll-cli/tests/code.rs", "modify", "@@ -12,0 +13,5 @@\n+#[test]\n+fn status_of_an_empty_repo() {\n+    let out = roll([\"code\", \"status\", \"--json\"]);\n+    assert_eq!(out[\"head\"], serde_json::Value::Null);\n+}\n")], ms: 900 } },
      {
        ask: {
          action: "command",
          title: "Run `cargo test -p roll-cli code`",
          command: "cargo test -p roll-cli code",
          reason: "Run the new test",
          decisions: ["accept", "accept_for_session", "decline", "cancel"],
        },
      },
      { tool: { name: "Bash", kind: "command", title: "cargo test -p roll-cli code", output: "running 10 tests\ntest status_of_an_empty_repo ... ok\n\ntest result: ok. 10 passed\n", exit: 0 } },
      { say: "Added `status_of_an_empty_repo`; it passes." },
    ],
  },
  {
    prompt: "Tidy the README's quick start",
    steps: [
      { think: "Shorter steps, one command each." },
      { tool: { name: "Edit", kind: "edit", title: "README.md", files: [fc("README.md", "modify", "@@ -3,1 +3,1 @@\n-Run the following commands in order to get started:\n+Get started:\n")], ms: 800 } },
      { say: "The quick start is three commands now." },
    ],
  },
];

const followUps: Record<string, string[]> = {
  default: [
    "Done. I kept the change small and added a test for it.",
    "Looked into it: the behaviour comes from the retry loop in `lander.rs`. I left a note there and didn't change anything else.",
  ],
};

// ── the simulation ──

interface Sim {
  thread: ThreadId;
  run: string;
  stopped: boolean;
  answer?: (r: RequestResponse) => void;
  /** A `/compact` run: steering it queues instead (T3). */
  compacting?: boolean;
}

const now = () => Date.now();
let seq = 0;
const id = (prefix: string) => `${prefix}demo${(++seq).toString(36).padStart(6, "0")}`;

export class DemoOwner implements AgentsTransport {
  private cursor = 0;
  private machines = new Map<MachineId, Machine>();
  private records = new Map<string, Mirrored>();
  private log: StreamEvent[] = [];
  private subs = new Set<{ threads: Set<string>; on: (ev: StreamEvent, id: number | null) => void }>();
  private sims = new Map<ThreadId, Sim>();
  /** Delegated threads still working: the orchestrator they report to and its task node. */
  private tasks = new Map<ThreadId, { parent: ThreadId; node: Node; title: string }>();
  /** Results waiting for a busy orchestrator's turn to end (the daemon's queued wake). */
  private wakes = new Map<ThreadId, string[]>();
  private timers = new Set<ReturnType<typeof setTimeout>>();
  private started = false;
  /** The demo's canvas arrangement: kept for as long as the page is open. */
  private canvas = memoryCanvas();
  /** The demo owner's agents settings, the same. */
  private settings = memorySettings();
  /** The demo owner's Slack, Discord and GitHub, the same. */
  private integrations = demoIntegrations();
  /** The demo owner's Linear workspace (connected). */
  private linear = memoryLinear(demoLinear(), DEMO_TEAMS);
  /** Every provider each demo machine has, on or off (its info lists the ones on). */
  private allProviders = new Map<MachineId, ProviderInfo[]>();
  /** The demo's automations and their runs (`automations-demo.ts`): runs start threads here. */
  private automations = new DemoAutomations({
    emit: (ev) => this.emit(ev),
    next: () => ++this.cursor,
    machines: () => (this.start(), [...this.machines.values()]),
    projects: () =>
      [...this.records.values()]
        .filter((r) => r.entity.kind === "project")
        .map((r) => {
          const p = r.entity as unknown as Project;
          return { id: p.id, name: p.name, machine: r.machine, repo: p.repo ? `${p.repo.owner}/${p.repo.repo}` : null };
        }),
    command: (machine, c) => this.command(machine, `auto_${Date.now()}`, c),
    threadStatus: (tid) => this.get<Thread>("thread", tid)?.status,
    after: (ms, fn) => this.after(ms, fn),
  });
  /**
   * `speed` scales every delay (tests run the demo fast); `keepAlive` starts new work whenever
   * the canvas goes quiet; `empty` starts with no machines (`?demo=empty`: the first-run screen).
   */
  constructor(
    private speed = 1,
    private keepAlive = true,
    private empty = false,
  ) {}

  // ── records ──

  private get<T>(kind: Entity["kind"], rid: string): T | undefined {
    const m = this.records.get(`${kind}:${rid}`);
    return m ? (m.entity as unknown as T) : undefined;
  }

  private emit(ev: StreamEvent) {
    if (ev.type !== "delta") {
      this.log.push(ev);
      if (this.log.length > 4000) this.log.splice(0, 1000);
    }
    for (const s of this.subs) if (this.visible(ev, s.threads)) s.on(ev, "cursor" in ev ? ev.cursor : null);
  }

  private visible(ev: StreamEvent, threads: Set<string>) {
    if (ev.type === "delta") return ev.delta.type === "terminal" || threads.has(ev.delta.thread);
    if (ev.type !== "entity") return true;
    const e = ev.entity;
    if (e.kind === "node" || e.kind === "checkpoint") return threads.has(e.thread);
    return true;
  }

  private put<K extends Exclude<Entity["kind"], "removed">>(kind: K, value: Extract<Entity, { kind: K }> extends infer E ? Omit<E, "kind"> : never, machine: MachineId) {
    const entity = { kind, ...value } as unknown as Entity;
    const m: Mirrored = { cursor: ++this.cursor, machine, entity };
    this.records.set(`${kind}:${entity.id}`, m);
    this.emit({ type: "entity", ...m });
  }

  private remove(kind: Exclude<Entity["kind"], "removed">, rid: string, thread: ThreadId | null) {
    const m = this.records.get(`${kind}:${rid}`);
    if (!m) return;
    this.records.delete(`${kind}:${rid}`);
    this.emit({ type: "entity", cursor: ++this.cursor, machine: m.machine, entity: { kind: "removed", entity: kind, id: rid, thread } });
  }

  private machineOf(thread: ThreadId): MachineId {
    return this.records.get(`thread:${thread}`)?.machine ?? MBP;
  }

  private thread(tid: ThreadId) {
    return this.get<Thread>("thread", tid);
  }

  private patchThread(tid: ThreadId, patch: Partial<Thread>) {
    const t = this.thread(tid);
    if (!t) return;
    const { kind: _k, ...rest } = t as Thread & { kind?: string };
    const next: Thread = { ...rest, ...patch, updated_at: now() };
    // A request or a failure ends a snooze early, as the daemon does.
    if (next.inbox.snooze && ((patch.pending_requests ?? 0) > 0 || patch.status === "error")) next.inbox = { ...next.inbox, snooze: null, woke_at: now() };
    this.put("thread", next, this.machineOf(tid));
  }

  /** The inbox changed. `touch: false` keeps `updated_at` (opening a conversation isn't activity). */
  private patchInbox(tid: ThreadId, patch: Partial<Thread["inbox"]>, touch = true) {
    const t = this.thread(tid);
    if (!t) return;
    const { kind: _k, ...rest } = t as Thread & { kind?: string };
    this.put("thread", { ...rest, inbox: { ...t.inbox, ...patch }, updated_at: touch ? now() : t.updated_at }, this.machineOf(tid));
  }

  /** What keeps a thread in the inbox whatever the user wants (the daemon's checks). */
  private parkable(t: Thread, what: string, running: boolean): Reply | null {
    const no = (why: string): Reply => ({ type: "error", code: "conflict", message: `the thread ${why} before ${what}` });
    if (t.archived_at) return no("is archived: unarchive it");
    if (t.pending_requests > 0) return no("is waiting on an approval or question: answer it");
    if (t.queued.length) return no("has a queued message: send it or remove it from the queue");
    if (running && t.status !== "idle" && t.status !== "error") return no("is still running: stop it");
    return null;
  }

  /** Wake a snooze when its time comes (the daemon's sweep). */
  private wakeAt(tid: ThreadId, until: number) {
    const ms = until - now();
    if (ms > 2 ** 31 - 1) return;
    this.after(Math.max(0, ms), () => {
      if (this.thread(tid)?.inbox.snooze?.until === until) this.patchInbox(tid, { snooze: null, woke_at: until });
    });
  }

  /** Auto-pull's rules, pretended: a dirty checkout is skipped, a clean one comes up to date. */
  private demoPull(pid: string, machine: MachineId): PullResult | null {
    const p = this.get<Project>("project", pid);
    if (!p) return null;
    const branch = p.git?.branch ?? "main";
    const outcome: PullOutcome = p.git?.dirty
      ? { status: "skipped", reason: "local_changes", message: "the checkout has uncommitted changes" }
      : p.pull?.outcome.status === "pulled" || p.pull?.outcome.status === "up_to_date"
        ? { status: "up_to_date", branch }
        : { status: "pulled", branch, from: p.git?.head ?? "f35f34cf", to: "9fceb02d", commits: 2 };
    const pull = { at: now(), outcome };
    const { kind: _k, ...rest } = p as Project & { kind?: string };
    const git = p.git && outcome.status === "pulled" ? { ...p.git, head: outcome.to } : p.git;
    this.put("project", { ...rest, git, pull, updated_at: now() }, machine);
    return pull;
  }

  private patchRun(rid: string, patch: Partial<Run>) {
    const r = this.get<Run & { kind: string }>("run", rid);
    if (!r) return;
    const { kind: _k, ...rest } = r;
    this.put("run", { ...rest, ...patch }, this.machineOf(r.thread));
  }

  private putNode(n: Node) {
    this.put("node", n, this.machineOf(n.thread));
  }

  // ── transport ──

  async state(threads: readonly ThreadId[]): Promise<State> {
    this.start();
    const want = new Set(threads);
    const entities = [...this.records.values()].filter((m) => {
      const e = m.entity;
      return !(e.kind === "node" || e.kind === "checkpoint") || want.has(e.thread);
    });
    return { cursor: this.cursor, machines: [...this.machines.values()], entities: entities.sort((a, b) => a.cursor - b.cursor) };
  }

  stream(threads: readonly ThreadId[], after: () => number, on: (ev: StreamEvent, id: number | null) => void, onOpen: (open: boolean) => void) {
    this.start();
    const sub = { threads: new Set(threads), on };
    const from = after();
    for (const ev of this.log) if ("cursor" in ev && ev.cursor > from && this.visible(ev, sub.threads)) on(ev, ev.cursor);
    this.subs.add(sub);
    const t = setTimeout(() => onOpen(true), 0);
    return () => {
      clearTimeout(t);
      this.subs.delete(sub);
    };
  }

  async command(machine: MachineId, _id: string, command: Command): Promise<Reply> {
    await this.sleep(120);
    // Like the relay: a command to an asleep (or stopped) sandbox wakes it and waits for it.
    const m = this.machines.get(machine);
    if (m && !m.online && reachable(m)) await this.wake(machine);
    if (!this.machines.get(machine)?.online) return { type: "error", code: "machine_offline", message: "the machine isn't connected" };
    try {
      return this.handle(machine, command);
    } catch (e) {
      return { type: "error", code: "invalid", message: e instanceof Error ? e.message : String(e) };
    }
  }

  /** Start an asleep sandbox (`starting` on the stream) and wait until it's connected again. */
  private async wake(machine: MachineId) {
    const s = [...this.sandboxes.values()].find((x) => x.machine === machine);
    if (!s || s.desired === "deleted") return;
    this.lastBusy.set(machine, now());
    if (s.status === "stopped") {
      this.setSandbox(s.id, { status: "starting", desired: "running", stopped_reason: null, last_active_at: now() });
      this.after(2500, () => this.setSandbox(s.id, { status: "running" }, { online: true, last_seen: now() }));
    }
    for (let i = 0; i < 40 && !this.machines.get(machine)?.online; i++) await this.sleep(150);
  }

  /** Sandboxes nothing runs on go to sleep, like the relay's reconciler does after `ROLL_SANDBOX_IDLE_STOP`. */
  private nap() {
    this.after(2000, () => {
      const t = now();
      for (const s of this.sandboxes.values()) {
        if (s.status !== "running" || s.desired !== "running" || !this.machines.get(s.machine)?.online) continue;
        const busy = [...this.records.values()].some((r) => r.machine === s.machine && r.entity.kind === "thread" && (r.entity.status === "starting" || r.entity.status === "running"));
        const since = this.lastBusy.get(s.machine);
        if (busy || since === undefined) this.lastBusy.set(s.machine, t);
        else if (t - since >= DEMO_IDLE_MS * this.speed) this.setSandbox(s.id, { status: "stopped", desired: "asleep", stopped_reason: "idle", last_active_at: since }, { online: false, last_seen: t });
      }
      this.nap();
    });
  }

  /** Stop every timer (tests, and leaving the page). */
  dispose() {
    for (const t of this.timers) clearTimeout(t);
    this.timers.clear();
    for (const s of this.sims.values()) s.stopped = true;
  }

  private sleep(ms: number) {
    return new Promise<void>((resolve) => {
      const t = setTimeout(() => {
        this.timers.delete(t);
        resolve();
      }, ms * this.speed);
      this.timers.add(t);
    });
  }

  // ── services: sandboxes, repos, work items (what `agents-services.ts` asks the API for) ──

  private sandboxes = new Map<string, Sandbox>();
  /** When each sandbox machine last had a thread starting or running (it sleeps a while after). */
  private lastBusy = new Map<MachineId, number>();

  private sandboxMachine(s: Sandbox, patch: Partial<Machine>) {
    const m = this.machines.get(s.machine);
    if (!m) return;
    const sandbox: SandboxInfo = { backend: s.backend, size: s.size.name, status: s.status, hourly_cents: s.size.hourly_cents };
    if (s.status === "stopped") sandbox.stopped_reason = s.desired === "stopped" ? "owner" : "idle";
    const next: Machine = { ...m, ...patch, sandbox };
    this.machines.set(m.id, next);
    this.emit({ type: "machine", cursor: ++this.cursor, machine: next });
  }

  private setSandbox(sid: string, patch: Partial<Sandbox>, machine: Partial<Machine> = {}) {
    const s = this.sandboxes.get(sid);
    if (!s) return undefined;
    const next = { ...s, ...patch, updated_at: now() };
    this.sandboxes.set(sid, next);
    this.sandboxMachine(next, machine);
    return next;
  }

  /** The demo's answers to `AgentsServices`, in memory like everything else here. */
  services(): AgentsServices {
    const owner = this;
    const sandbox = (sid: string) => {
      const s = owner.sandboxes.get(sid);
      if (!s) throw new ApiError("no such sandbox", 404, null);
      return s;
    };
    return {
      automations: owner.automations.api(),
      sandboxes: {
        async list() {
          owner.start();
          return { sandboxes: [...owner.sandboxes.values()].filter((s) => s.desired !== "deleted"), sizes: DEMO_SIZES, max: 5, backend: "agent37" };
        },
        async create(_o, req = {}) {
          owner.start();
          if ([...owner.sandboxes.values()].filter((s) => s.desired !== "deleted").length >= 5) throw new ApiError("at the limit of 5 sandboxes", 422, null);
          const size = DEMO_SIZES.find((z) => z.name === req.size) ?? DEMO_SIZES[0];
          const sid = id("sbx_");
          const mid = id("mch_");
          const name = req.name?.trim() || `sandbox-${owner.sandboxes.size + 1}`;
          const s: Sandbox = { id: sid, name, machine: mid, backend: "agent37", size, status: "creating", desired: "running", stopped_reason: null, error: null, created_at: now(), updated_at: now(), last_active_at: now() };
          owner.sandboxes.set(sid, s);
          owner.machines.set(mid, { id: mid, name, online: false, info: null, sandbox: null, appearance: {}, created_at: now(), last_seen: null });
          owner.sandboxMachine(s, {});
          const keys = new Set(Object.keys(req.env ?? {}));
          // It boots for a few seconds, then connects like any machine.
          owner.after(4500, () =>
            owner.setSandbox(
              sid,
              { status: "running" },
              {
                online: true,
                last_seen: now(),
                info: {
                  hostname: name,
                  os: "linux",
                  arch: "x86_64",
                  version: "0.1.0",
                  home: "/home/roll",
                  ...owner.catalog(mid, "/home/roll", [claude(keys.has("ANTHROPIC_API_KEY") ? "ready" : "missing"), codex(keys.has("OPENAI_API_KEY") ? "ready" : "missing")]),
                },
              },
            ),
          );
          return { sandbox: s, machine: owner.machines.get(mid)! };
        },
        async start(_o, sid) {
          sandbox(sid);
          owner.lastBusy.set(sandbox(sid).machine, now());
          owner.after(1500, () => owner.setSandbox(sid, {}, { online: true, last_seen: now() }));
          return owner.setSandbox(sid, { status: "running", desired: "running", stopped_reason: null, last_active_at: now() })!;
        },
        async stop(_o, sid) {
          sandbox(sid);
          return owner.setSandbox(sid, { status: "stopped", desired: "stopped", stopped_reason: "owner" }, { online: false, last_seen: now() })!;
        },
        async remove(_o, sid) {
          const s = sandbox(sid);
          owner.setSandbox(sid, { status: "deleting", desired: "deleted" }, { online: false });
          owner.after(1500, () => {
            owner.machines.delete(s.machine);
            owner.emit({ type: "machine_removed", cursor: ++owner.cursor, machine: s.machine });
          });
          return owner.sandboxes.get(sid)!;
        },
        async usage(_o, month) {
          const rows = [...owner.sandboxes.values()].map((s) => {
            const hours = s.status === "running" ? Math.max(0.01, (now() - s.created_at) / 3_600_000) : 0;
            return {
              sandbox: s.id,
              name: s.name,
              size: s.size.name,
              hourly_cents: s.size.hourly_cents,
              running_ms: Math.round(hours * 3_600_000),
              running_hours: hours,
              price_usd: (hours * s.size.hourly_cents) / 100,
            };
          });
          const sum = (f: (r: (typeof rows)[number]) => number) => rows.reduce((a, r) => a + f(r), 0);
          return {
            owner: "demo",
            month: month ?? new Date().toISOString().slice(0, 7),
            sandboxes: rows,
            totals: { running_ms: sum((r) => r.running_ms), running_hours: sum((r) => r.running_hours), price_usd: sum((r) => r.price_usd) },
          };
        },
      },
      async repos() {
        return DEMO_REPOS;
      },
      async createProject(_o, machine, req) {
        await owner.sleep(600);
        const home = owner.machines.get(machine)?.info?.home ?? "/home/roll";
        return owner.command(machine, req.id, { type: "project.add", path: req.path || `${home}/roll/acme/${req.repo.name}`, name: req.name, clone: { owner: "acme", repo: req.repo.name } });
      },
      async takeWork(_o, thread, req) {
        if (!owner.thread(thread)) throw new ApiError("no such thread", 404, null);
        const given = "work" in req ? req.work : null;
        const repo = given?.includes("#") ? given.split("#")[0] : "repo" in req ? req.repo : "acme/roll";
        const number = given?.includes("#") ? Number(given.split("#")[1]) : 60 + (seq % 20);
        const item = { id: `wi_demo${number}`, repo, number, title: given ? "Make `roll agent status` show sandboxes" : "Speed up the lander's conflict check" };
        owner.patchThread(thread, { work: item });
        return item;
      },
      async releaseWork(_o, thread) {
        owner.patchThread(thread, { work: null });
      },
      machines: {
        async rename(_o, mid, name) {
          const m = owner.machines.get(mid);
          if (!m) throw new ApiError("no such machine", 404, null);
          const next = { ...m, name: name.trim() || m.name };
          owner.machines.set(mid, next);
          owner.emit({ type: "machine", cursor: ++owner.cursor, machine: next });
          return next;
        },
        async style(_o, mid, style) {
          const m = owner.machines.get(mid);
          if (!m) throw new ApiError("no such machine", 404, null);
          const appearance = { ...m.appearance };
          if (style.color !== undefined) appearance.color = style.color ?? undefined;
          if (style.kind !== undefined) appearance.kind = style.kind ?? undefined;
          const next = { ...m, appearance };
          owner.machines.set(mid, next);
          owner.emit({ type: "machine", cursor: ++owner.cursor, machine: next });
          return next;
        },
        async remove(_o, mid) {
          if (!owner.machines.delete(mid)) throw new ApiError("no such machine", 404, null);
          owner.emit({ type: "machine_removed", cursor: ++owner.cursor, machine: mid });
        },
      },
      canvas: owner.canvas,
      settings: owner.settings,
      integrations: owner.integrations,
      linear: owner.linear,
    };
  }

  /** A demo machine's providers and settings (none set yet), keeping the full list for `machine.configure`. */
  private catalog(mid: MachineId, home: string, all: ProviderInfo[], fileEnv?: Record<string, string[]>) {
    this.allProviders.set(mid, all);
    return demoConfig(home, all, {}, fileEnv);
  }

  // ── seed ──

  private start() {
    if (this.started) return;
    this.started = true;
    if (this.empty) return;
    const t0 = now() - 3_600_000;
    this.machines.set(MBP, {
      id: MBP,
      name: "ada-mbp",
      online: true,
      info: { hostname: "ada-mbp", os: "macos", arch: "aarch64", version: "0.1.0", home: "/Users/ada", ...this.catalog(MBP, "/Users/ada", [claude(), codex(), gemini, grok], { claude: ["ANTHROPIC_API_KEY"] }) },
      sandbox: null,
      // Picked in its settings (the rest of the demo shows colours made from the record).
      appearance: { color: "slate" },
      created_at: t0,
      last_seen: now(),
    });
    this.machines.set(BOX, {
      id: BOX,
      name: "build-box",
      online: true,
      info: { hostname: "build-box", os: "linux", arch: "x86_64", version: "0.1.0", home: "/home/roll", ...this.catalog(BOX, "/home/roll", [codex(), claude("missing")]) },
      sandbox: { backend: "agent37", size: "standard", status: "running", hourly_cents: 3 },
      appearance: {},
      created_at: t0,
      last_seen: now(),
    });
    this.sandboxes.set("sbx_demo_box", {
      id: "sbx_demo_box",
      name: "build-box",
      machine: BOX,
      backend: "agent37",
      size: DEMO_SIZES[0],
      status: "running",
      desired: "running",
      stopped_reason: null,
      error: null,
      created_at: t0 - 6 * 86_400_000,
      updated_at: t0,
      last_active_at: t0,
    });
    this.cursor = 2;
    this.nap();

    const project = (pid: string, name: string, path: string, branch: string, dirty: boolean, repo: string | null, over: Partial<Project> = {}): Project => ({
      id: pid,
      name,
      path,
      repo: repo ? { owner: "acme", repo } : null,
      git: { branch, head: "f35f34cf", remote: repo ? `https://roll.sh/acme/${repo}.git` : null, dirty },
      settings: { ...NO_SETTINGS },
      pull: null,
      logo: null,
      created_at: t0,
      updated_at: t0,
      ...over,
    });
    this.put(
      "project",
      project("prj_demo_roll", "roll", "/Users/ada/roll", "main", true, "roll", {
        settings: {
          auto_pull: true,
          defaults: { provider: "claude", runtime_mode: "full_access", worktree: true },
          scripts: [
            { id: "setup", name: "Install", command: "pnpm install", icon: "configure", on_worktree: true },
            { id: "test", name: "Test", command: "cargo test -p roll-cli", icon: "test", on_worktree: false },
          ],
          auto_settle: DEFAULT_AUTO_SETTLE,
        },
        // What the daemon finds in the roll repo: its favicon (the Horizon on its tile).
        logo: { path: "public/favicon.svg", url: svgUrl(avatarSvg({ size: 64 })) },
        pull: {
          at: t0 - 6 * 60_000,
          outcome: { status: "skipped", reason: "local_changes", message: "the checkout has uncommitted changes" },
        },
      }),
      MBP,
    );
    this.put(
      "project",
      project("prj_demo_web", "web", "/Users/ada/web", "main", false, "web", {
        settings: { ...NO_SETTINGS, auto_pull: true },
        pull: { at: t0 - 14 * 60_000, outcome: { status: "pulled", branch: "main", from: "8ea39e4d", to: "f35f34cf", commits: 3 } },
      }),
      MBP,
    );
    this.put("project", project("prj_demo_infra", "infra", "/home/roll/infra", "main", false, "infra", { settings: { ...NO_SETTINGS, color: "teal", icon: { kind: "emoji", emoji: "🧰" } } }), BOX);

    const base = (tid: string, pid: string, title: string, provider: string, model: string, over: Partial<Thread> = {}): Omit<Thread, never> => ({
      id: tid,
      project: pid,
      title,
      provider,
      model,
      effort: provider === "claude" ? "high" : "medium",
      options: {},
      runtime_mode: "full_access",
      interaction_mode: "default",
      cwd: `/Users/ada/${pid === "prj_demo_web" ? "web" : "roll"}`,
      worktree: null,
      status: "idle",
      lineage: null,
      orchestrator: false,
      active_run: null,
      queued: [],
      run_count: 0,
      pending_requests: 0,
      activity: null,
      usage: { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null },
      context: null,
      error: null,
      change: null,
      work: null,
      created_at: t0,
      updated_at: t0,
      archived_at: null,
      inbox: { ...EMPTY_INBOX },
      ...over,
    });

    // A finished thread with history, and a fork of it.
    const done = base("thr_demo_json", "prj_demo_roll", "Add --json to roll code status", "codex", "gpt-5.5-codex", {
      created_at: t0 + 1000,
      change: { repo: "acme/roll", number: 41, status: "ready", url: "/acme/roll/changes/41", detail: "All 3 checks passed" },
      work: { id: "wi_demo38", repo: "acme/roll", number: 38, title: "roll code status --json" },
      worktree: { path: "/Users/ada/.local/share/roll/agent/worktrees/prj_demo_roll/thr_demo_json", branch: "roll/add-json-to-status", base: "f35f34cf" },
    });
    this.put("thread", done, MBP);
    this.completedRun("thr_demo_json", 1, "Add a --json flag to `roll code status`", "Added `--json` to `roll code status`: it prints the same fields as the table, pretty-printed. `roll-cli/tests/code.rs` covers both outputs.", [fc("crates/roll-cli/src/code/status.rs", "modify", DIFF_STATUS)], t0 + 2000);
    this.put(
      "thread",
      base("thr_demo_fork", "prj_demo_roll", "Try a compact table instead", "claude", "claude-sonnet-5", {
        created_at: t0 + 5000,
        lineage: { parent: "thr_demo_json", relation: "fork", root: "thr_demo_json", from_run: "run_demo_json1" },
      }),
      MBP,
    );

    // The busy ones.
    this.put("thread", base("thr_demo_flaky", "prj_demo_roll", "Fix the flaky queue test", "claude", "claude-opus-5-5", { created_at: t0 + 9000 }), MBP);
    this.put("thread", base("thr_demo_next", "prj_demo_web", "Upgrade to Next 16.4", "claude", "claude-opus-5-5", { created_at: t0 + 8000 }), MBP);
    this.put("thread", base("thr_demo_ci", "prj_demo_infra", "Move CI to roll checks", "codex", "gpt-5.5-codex", { created_at: t0 + 7000, interaction_mode: "plan", cwd: "/home/roll/infra" }), BOX);
    this.plannedRun(base("thr_demo_billing", "prj_demo_web", "Plan the CSV invoice export", "codex", "gpt-5.5-codex", { created_at: t0 + 6000, interaction_mode: "plan" }));
    this.put("thread", base("thr_demo_old", "prj_demo_web", "Spike: edge middleware", "codex", "gpt-5.5", { created_at: t0, archived_at: t0 + 100 }), MBP);

    // The inbox: settled threads (by hand, after days without activity, a landed change),
    // a snoozed one, and one whose snooze just woke.
    const DAY = 86_400_000;
    const shelved = (tid: string, pid: string, title: string, provider: string, model: string, ago: number, inbox: Partial<Thread["inbox"]>, over: Partial<Thread> = {}) => {
      const at = now() - ago;
      const machine = pid === "prj_demo_infra" ? BOX : MBP;
      this.put("thread", base(tid, pid, title, provider, model, { created_at: at - 600_000, updated_at: at, run_count: 1, inbox: { ...EMPTY_INBOX, visited_at: at, ...inbox }, ...over }), machine);
      const usage = { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null };
      this.put("run", { id: `run_${tid}`, thread: tid, ordinal: 1, provider, model, status: "completed", prompt: title, attachments: [], steers: [], requested_at: at - 600_000, started_at: at - 600_000, completed_at: at, checkpoint: null, stat: null, usage, context: null, error: null }, machine);
      this.putNode({ id: `nod_${tid}`, thread: tid, run: `run_${tid}`, parent: null, ordinal: 1, status: "completed", started_at: at - 1000, completed_at: at, body: { type: "message", text: `Done: ${title.toLowerCase()}.` } });
    };
    const settled = (by: "user" | "inactivity" | "landed", ago: number) => ({ settle: { state: "settled" as const, at: now() - ago, by } });
    shelved("thr_demo_tokio", "prj_demo_roll", "Bump tokio to 1.48", "codex", "gpt-5.5-codex", 2 * DAY, settled("landed", 2 * DAY), {
      change: { repo: "acme/roll", number: 39, status: "landed", url: "/acme/roll/changes/39", detail: null },
    });
    shelved("thr_demo_docs", "prj_demo_roll", "Document the merge queue", "claude", "claude-opus-5-5", 4 * DAY, settled("inactivity", 4 * DAY));
    shelved("thr_demo_zoom", "prj_demo_web", "Tune the canvas zoom curve", "claude", "claude-sonnet-5", 26 * 3_600_000, settled("user", 26 * 3_600_000));
    const older = ["Rename the queue metrics", "Drop the legacy blob cache", "Fix a typo in the CLI help", "Trim the Docker image", "Pin the Rust toolchain", "Cache pnpm in CI", "Split the e2e suite", "Retry flaky clone tests", "Add a --quiet flag", "Upgrade Geist", "Fix the 404 page", "Tidy the README"];
    older.forEach((title, i) => {
      const ago = (6 + i * 2) * DAY;
      shelved(`thr_demo_old${i}`, i % 3 === 2 ? "prj_demo_web" : "prj_demo_roll", title, i % 2 ? "claude" : "codex", i % 2 ? "claude-sonnet-5" : "gpt-5.5-codex", ago, settled("inactivity", ago));
    });
    const tomorrow = new Date(now() + DAY);
    tomorrow.setHours(9, 0, 0, 0);
    shelved("thr_demo_e2e", "prj_demo_web", "Review the flaky e2e retries", "codex", "gpt-5.5", 3 * 3_600_000, { snooze: { until: tomorrow.getTime(), at: now() - 3 * 3_600_000 } });
    shelved("thr_demo_keys", "prj_demo_infra", "Rotate the staging keys", "claude", "claude-opus-5-5", 2 * DAY, { visited_at: now() - 2 * DAY, woke_at: now() - 20 * 60_000 });

    if (this.keepAlive) this.idle();
    this.after(400, () => this.begin("thr_demo_flaky", "The queue test fails one time in ten. Find out why and fix it.", flakyScript));
    this.after(1200, () => this.begin("thr_demo_next", "Upgrade the web app to Next 16.4", nextScript));
    this.after(2000, () => this.begin("thr_demo_ci", "Move our CI from GitHub Actions to roll checks", ciScript));
  }

  /** When fewer than two threads are working, give an idle one something to do. */
  private idle(turn = 0) {
    this.after(7000, () => {
      if (this.sims.size < 2) {
        const idle = [...this.records.values()]
          .filter((m) => m.entity.kind === "thread" && m.entity.status === "idle" && m.entity.interaction_mode !== "plan" && m.entity.archived_at === null && !m.entity.inbox.settle && !m.entity.inbox.snooze && this.machines.get(m.machine)?.online)
          .map((m) => m.entity.id);
        const tid = idle[turn % Math.max(1, idle.length)];
        const run = autoRuns[turn % autoRuns.length];
        if (tid) this.begin(tid, run.prompt, run.steps);
      }
      this.idle(turn + 1);
    });
  }

  private after(ms: number, fn: () => void) {
    void this.sleep(ms).then(fn);
  }

  /**
   * A thread in plan mode whose run ended with a plan (Codex: no approval), after Codex compacted
   * the context on its own mid-run: the composer shows "Plan ready" with Implement, and the
   * transcript the compaction row.
   */
  private plannedRun(thread: Thread) {
    const at = now() - 25 * 60_000;
    const tid = thread.id;
    const rid = "run_demo_billing1";
    this.put("thread", { ...thread, updated_at: at + 140_000 }, MBP);
    const usage = { input_tokens: 412_000, output_tokens: 9_400, cached_tokens: 301_000, cost_usd: null };
    const context: ContextUsage = { used_tokens: 61_900, max_tokens: CODEX_WINDOW, total_processed_tokens: 503_400 };
    this.put("run", { id: rid, thread: tid, ordinal: 1, provider: "codex", model: "gpt-5.5-codex", status: "completed", prompt: "Plan a CSV export for invoices: the API route, a button in the dashboard, and tests.", attachments: [], steers: [], requested_at: at, started_at: at, completed_at: at + 140_000, checkpoint: null, stat: null, usage, context, error: null }, MBP);
    let o = 0;
    const node = (body: NodeBody, ms: number) => this.putNode({ id: id("nod_"), thread: tid, run: rid, parent: null, ordinal: ++o, status: "completed", started_at: at + ms, completed_at: at + ms + 4000, body });
    const read = (path: string, ms: number) => node({ type: "tool", name: "shell", kind: "read", title: `sed -n 1,400p ${path}`, input: null, output: "…", exit_code: 0, files: [] }, ms);
    node({ type: "reasoning", text: "Find where invoices are listed and how the billing routes stream rows." }, 2000);
    read("crates/billing/src/http.rs", 8000);
    read("crates/billing/src/invoices.rs", 16_000);
    read("apps/dashboard/src/components/billing/invoices.tsx", 30_000);
    node({ type: "compaction", before_tokens: 238_600, after_tokens: 52_300, error: null }, 52_000);
    read("crates/billing/tests/http.rs", 90_000);
    node({ type: "plan", text: BILLING_PLAN }, 120_000);
    node({ type: "message", text: "That's the plan. Say **Implement** to carry it out, or tell me what to change." }, 136_000);
    this.patchThread(tid, { run_count: 1, usage, context, activity: null, updated_at: at + 140_000 });
  }

  private completedRun(tid: ThreadId, ordinal: number, prompt: string, answer: string, files: FileChange[], at: number) {
    const rid = `run_demo_json${ordinal}`;
    const s = stat(files);
    const cid = `chk_demo_json${ordinal}`;
    const usage = { input_tokens: 41_200, output_tokens: 3_900, cached_tokens: 30_100, cost_usd: null };
    const context: ContextUsage = { used_tokens: 31_800, max_tokens: CODEX_WINDOW, total_processed_tokens: 45_100 };
    this.put("run", { id: rid, thread: tid, ordinal, provider: "codex", model: "gpt-5.5-codex", status: "completed", prompt, attachments: [], steers: [], requested_at: at, started_at: at, completed_at: at + 52_000, checkpoint: cid, stat: s, usage, context, error: null }, MBP);
    let o = 0;
    const node = (body: NodeBody) => this.putNode({ id: id("nod_"), thread: tid, run: rid, parent: null, ordinal: ++o, status: "completed", started_at: at, completed_at: at + 1000, body });
    node({ type: "reasoning", text: "The status command builds a `Status` struct and prints a table; serde is already a dependency." });
    node({ type: "tool", name: "shell", kind: "read", title: "sed -n 1,120p crates/roll-cli/src/code/status.rs", input: null, output: "pub struct StatusArgs {\n…", exit_code: 0, files: [] });
    node({ type: "tool", name: "apply_patch", kind: "edit", title: "crates/roll-cli/src/code/status.rs", input: null, output: null, exit_code: null, files });
    node({ type: "tool", name: "shell", kind: "command", title: "cargo test -p roll-cli code::status", input: null, output: "test result: ok. 9 passed; 0 failed\n", exit_code: 0, files: [] });
    node({ type: "message", text: answer });
    this.put("checkpoint", { id: `chk_demo_json0`, thread: tid, run: null, ordinal: 0, commit: "f35f34cf", stat: { files: 0, additions: 0, deletions: 0 }, created_at: at }, MBP);
    this.put("checkpoint", { id: cid, thread: tid, run: rid, ordinal, commit: "9fceb02d0ae598e9", stat: s, created_at: at + 52_000 }, MBP);
    this.patchThread(tid, { run_count: ordinal, usage, context, activity: null, updated_at: at + 52_000 });
  }

  // ── running a script ──

  /** A message starts a run: `/compact` compacts where the provider can (T3), anything else gets a reply. */
  private sendMessage(tid: ThreadId, text: string, attachments: Run["attachments"] = []) {
    const t = this.thread(tid);
    if (!t) return;
    const caps = this.allProviders.get(this.machineOf(tid))?.find((p) => p.id === t.provider)?.capabilities;
    if (text.trim() === "/compact" && !attachments.length && caps?.compact) {
      this.begin(tid, text, [{ compact: {} }]);
      const sim = this.sims.get(tid);
      if (sim) sim.compacting = true;
      return;
    }
    this.begin(tid, text, this.reply(text, t.interaction_mode), attachments);
  }

  /** Start a run on an idle thread. */
  private begin(tid: ThreadId, prompt: string, script: Step[], attachments: Run["attachments"] = []) {
    const t = this.thread(tid);
    if (!t) return;
    const ordinal = t.run_count + 1;
    const rid = id("run_");
    const sim: Sim = { thread: tid, run: rid, stopped: false };
    this.sims.set(tid, sim);
    this.put(
      "run",
      { id: rid, thread: tid, ordinal, provider: t.provider, model: t.model, status: "running", prompt, attachments, steers: [], requested_at: now(), started_at: now(), completed_at: null, checkpoint: null, stat: null, usage: { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null }, context: null, error: null },
      this.machineOf(tid),
    );
    // Any new run is activity: off the settled shelf.
    this.patchThread(tid, { status: "running", active_run: rid, run_count: ordinal, activity: "Thinking", error: null, inbox: { ...t.inbox, settle: null } });
    void this.play(sim, script, { ordinal: 0, files: [] }).then(() => this.finish(sim));
  }

  private async play(sim: Sim, steps: Step[], ctx: { ordinal: number; files: FileChange[] }, parent: string | null = null): Promise<void> {
    for (const step of steps) {
      if (sim.stopped) return;
      // A subagent says what it's doing (Claude's `task_progress`).
      if (parent !== null) this.progress(parent, step);
      await this.step(sim, step, ctx, parent);
      // Subagents work in their own context; the thread's grows with its own steps, and past
      // 92 % of the window the provider compacts it (Claude and Codex do).
      if (parent === null && !sim.stopped && !("compact" in step) && this.growContext(sim)) await this.step(sim, { compact: {} }, ctx, null);
      await this.sleep(350);
    }
  }

  private progress(parent: string, step: Step) {
    const n = this.get<Node>("node", parent);
    if (n?.body.type !== "subagent" || !("tool" in step)) return;
    const t = step.tool;
    const line = t.kind === "read" ? `Reading ${t.title}` : t.kind === "search" ? `Searching ${t.title}` : t.kind === "command" ? `Running ${t.title}` : t.title;
    this.putNode({ ...n, body: { ...n.body, progress: line } });
  }

  /**
   * Each model call carries the whole conversation, so the context grows a few thousand tokens a
   * step (the first call starts with the system prompt and tools). Says whether it passed 92 % of
   * the window, where the provider compacts it.
   */
  private growContext(sim: Sim): boolean {
    const t = this.thread(sim.thread);
    if (!t) return false;
    const max = contextWindow(t.model);
    const before = t.context?.used_tokens ?? (t.provider === "claude" ? 21_400 : 13_800);
    const used = before + 1_500 + Math.floor(Math.random() * 6_000);
    const processed = t.provider === "codex" ? (t.context?.total_processed_tokens ?? 0) + used : null;
    const context: ContextUsage = { used_tokens: used, max_tokens: max, total_processed_tokens: processed };
    this.patchRun(sim.run, { context });
    this.patchThread(sim.thread, { context });
    return max !== null && used > max * 0.92;
  }

  private node(sim: Sim, ctx: { ordinal: number }, body: NodeBody, parent: string | null, status: Node["status"] = "running"): Node {
    const n: Node = { id: id("nod_"), thread: sim.thread, run: sim.run, parent, ordinal: ++ctx.ordinal, status, started_at: now(), completed_at: null, body };
    this.putNode(n);
    return n;
  }

  private done(n: Node, body: NodeBody = n.body, status: Node["status"] = "completed") {
    this.putNode({ ...n, body, status, completed_at: now() });
  }

  /** Stream `text` into a message/reasoning/plan node, re-upserting it every ~500 ms like the daemon. */
  private async pour(sim: Sim, n: Node, text: string, field: "text" | "output" = "text") {
    const machine = this.machineOf(sim.thread);
    let sent = 0;
    let lastUpsert = now();
    while (sent < text.length && !sim.stopped) {
      const take = Math.min(text.length - sent, 3 + Math.floor(Math.random() * 6));
      const chunk = text.slice(sent, sent + take);
      sent += take;
      this.emit({ type: "delta", machine, delta: { type: field, thread: sim.thread, node: n.id, text: chunk } });
      if (now() - lastUpsert > 500) {
        lastUpsert = now();
        const body = field === "text" ? { ...(n.body as { type: "message"; text: string }), text: text.slice(0, sent) } : { ...(n.body as Extract<NodeBody, { type: "tool" }>), output: text.slice(0, sent) };
        this.putNode({ ...n, body });
      }
      await this.sleep(field === "text" ? 32 : 60);
    }
  }

  private async step(sim: Sim, step: Step, ctx: { ordinal: number; files: FileChange[] }, parent: string | null) {
    const activity = (a: string | null) => this.patchThread(sim.thread, { activity: a });
    if ("say" in step || "think" in step || "plan" in step) {
      const [type, text] = "say" in step ? (["message", step.say] as const) : "think" in step ? (["reasoning", step.think] as const) : (["plan", step.plan] as const);
      if (!parent) activity(type === "message" ? "Writing" : type === "plan" ? "Planning" : "Thinking");
      const n = this.node(sim, ctx, { type, text: "" }, parent);
      await this.pour(sim, n, text);
      this.done(n, { type, text });
      return;
    }
    if ("todos" in step) {
      const existing = [...this.records.values()].find((m) => m.entity.kind === "node" && m.entity.run === sim.run && m.entity.body.type === "todos");
      if (existing) {
        const n = existing.entity as Node & { kind: string };
        const { kind: _k, ...rest } = n;
        const allDone = step.todos.every((t) => t.status === "completed");
        this.putNode({ ...rest, body: { type: "todos", items: step.todos }, status: allDone ? "completed" : "running", completed_at: allDone ? now() : null });
      } else this.node(sim, ctx, { type: "todos", items: step.todos }, parent);
      return;
    }
    if ("tool" in step) {
      const tl = step.tool;
      if (!parent) activity(tl.kind === "edit" ? `Editing ${tl.title}` : tl.kind === "command" ? `Running ${tl.title}` : tl.kind === "read" ? `Reading ${tl.title}` : tl.title);
      const n = this.node(sim, ctx, { type: "tool", name: tl.name, kind: tl.kind, title: tl.title, input: tl.input ?? null, output: null, exit_code: null, files: [] }, parent);
      if (tl.output && tl.kind === "command") await this.pour(sim, n, tl.output, "output");
      else await this.sleep(tl.ms ?? 800);
      if (sim.stopped) return;
      if (tl.files) ctx.files.push(...tl.files);
      this.done(n, { type: "tool", name: tl.name, kind: tl.kind, title: tl.title, input: tl.input ?? null, output: tl.output ?? null, exit_code: tl.exit ?? (tl.kind === "command" ? 0 : null), files: tl.files ?? [] }, tl.exit && tl.exit !== 0 ? "failed" : "completed");
      return;
    }
    if ("sub" in step) {
      if (!parent) activity(`Delegating to ${step.sub.agent}`);
      const body = { type: "subagent" as const, agent: step.sub.agent, prompt: step.sub.prompt, child: null, result: null, model: step.sub.model ?? null };
      const n = this.node(sim, ctx, body, parent);
      await this.play(sim, step.sub.steps, ctx, n.id);
      this.done(n, { ...body, result: step.sub.result, progress: null });
      return;
    }
    if ("compact" in step) {
      // T3's compaction row: running, then the window before → after; the meter drops.
      activity("Compacting context");
      const t = this.thread(sim.thread)!;
      const before = t.context?.used_tokens ?? null;
      const n = this.node(sim, ctx, { type: "compaction", before_tokens: before, after_tokens: null, error: null }, null);
      await this.sleep(1600);
      if (sim.stopped) return;
      const after = step.compact.after ?? Math.round((before ?? 40_000) * 0.24);
      const context: ContextUsage = { used_tokens: after, max_tokens: t.context?.max_tokens ?? contextWindow(t.model), total_processed_tokens: t.context?.total_processed_tokens ?? null };
      this.patchRun(sim.run, { context });
      this.patchThread(sim.thread, { context });
      this.done(n, { type: "compaction", before_tokens: before, after_tokens: after, error: null });
      return;
    }
    if ("ask" in step || "questions" in step) {
      const rid = id("req_");
      const body: Request["body"] =
        "ask" in step
          ? { type: "approval", command: null, cwd: null, reason: null, tool: null, files: [], ...step.ask, input: step.ask.input ?? null }
          : { type: "questions", questions: step.questions };
      const n = this.node(sim, ctx, { type: "request", request: rid }, parent, "waiting");
      const req: Request = { id: rid, thread: sim.thread, run: sim.run, node: n.id, status: "pending", body, created_at: now(), resolved_at: null, resolved_by: null, response: null };
      this.put("request", req, this.machineOf(sim.thread));
      const t = this.thread(sim.thread)!;
      this.patchThread(sim.thread, { status: "waiting", pending_requests: t.pending_requests + 1, activity: body.type === "approval" ? `Waiting: ${body.title}` : "Waiting for your answer" });
      this.patchRun(sim.run, { status: "waiting" });
      this.followTask(sim.thread, "waiting");
      const answer = await new Promise<RequestResponse>((resolve) => (sim.answer = resolve));
      this.followTask(sim.thread, "running");
      sim.answer = undefined;
      this.done(n, n.body);
      this.put("request", { ...req, status: "resolved", resolved_at: now(), resolved_by: "ada", response: answer }, this.machineOf(sim.thread));
      const t2 = this.thread(sim.thread)!;
      this.patchThread(sim.thread, { status: "running", pending_requests: Math.max(0, t2.pending_requests - 1), activity: "Working" });
      this.patchRun(sim.run, { status: "running" });
      if (answer.type === "decision" && (answer.decision === "decline" || answer.decision === "cancel")) {
        if (answer.decision === "cancel") {
          sim.stopped = true;
          this.stopRun(sim, "interrupted");
          return;
        }
        const m = this.node(sim, ctx, { type: "message", text: "" }, parent);
        const text = answer.message ? `Understood: ${answer.message}. I'll leave it there.` : "Okay, I won't run that. Stopping here.";
        await this.pour(sim, m, text);
        this.done(m, { type: "message", text });
        sim.stopped = true;
        this.finish({ ...sim, stopped: false });
      }
      return;
    }
    if ("wait" in step) await this.sleep(step.wait);
    if ("delegate" in step) {
      activity(`Delegating to ${step.delegate.length} agents`);
      const parent = this.thread(sim.thread)!;
      const machine = this.machineOf(sim.thread);
      const project = this.get<Project>("project", parent.project);
      for (const [i, task] of step.delegate.entries()) {
        const n = this.node(sim, ctx, { type: "subagent", agent: task.provider, prompt: task.task, child: null, result: null }, null);
        const tid = id("thr_");
        const path = project?.path ?? parent.cwd;
        this.put(
          "thread",
          {
            ...parent,
            id: tid,
            title: task.title,
            provider: task.provider,
            model: task.model,
            effort: null,
            interaction_mode: "default",
            cwd: `${path}/../.roll-worktrees/${tid}`,
            worktree: { path: `${path}/../.roll-worktrees/${tid}`, branch: `roll/${slug(task.title)}`, base: "f35f34cf" },
            status: "idle",
            lineage: { parent: parent.id, relation: "subagent", root: parent.lineage?.root ?? parent.id, from_run: sim.run },
            orchestrator: false,
            active_run: null,
            queued: [],
            run_count: 0,
            pending_requests: 0,
            activity: null,
            usage: { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null },
            context: null,
            error: null,
            change: null,
            work: null,
            created_at: now(),
            updated_at: now(),
            archived_at: null,
          },
          machine,
        );
        const node = { ...n, body: { type: "subagent" as const, agent: task.provider, prompt: task.task, child: tid, result: null } };
        this.putNode(node);
        this.tasks.set(tid, { parent: parent.id, node, title: task.title });
        this.after(300 + i * 900, () => this.begin(tid, task.task, task.steps));
        await this.sleep(250);
      }
    }
  }

  /** A delegated thread's task node follows it: waiting while it waits on you. */
  private followTask(tid: ThreadId, status: "running" | "waiting") {
    const task = this.tasks.get(tid);
    if (!task) return;
    task.node = { ...task.node, status };
    this.putNode(task.node);
  }

  /** A delegated thread finished: its node gets the result, and the result wakes the orchestrator. */
  private taskDone(tid: ThreadId, run: string) {
    const task = this.tasks.get(tid);
    if (!task) return;
    this.tasks.delete(tid);
    const answer = [...this.records.values()]
      .map((m) => m.entity)
      .filter((e): e is Node & { kind: "node" } => e.kind === "node" && e.run === run && e.parent === null && e.body.type === "message")
      .sort((a, b) => b.ordinal - a.ordinal)[0];
    const result = answer?.body.type === "message" ? answer.body.text : "Done.";
    this.putNode({ ...task.node, status: "completed", completed_at: now(), body: { ...(task.node.body as Extract<NodeBody, { type: "subagent" }>), result } });
    const wake = `Delegated task ${task.node.id} (${task.title}) [completed]:\n${result}`;
    if (this.sims.has(task.parent)) this.wakes.set(task.parent, [...(this.wakes.get(task.parent) ?? []), wake]);
    else this.wakeOrchestrator(task.parent, [wake]);
  }

  private wakeOrchestrator(parent: ThreadId, results: string[]) {
    const titles = results.map((r) => /\((.*?)\) \[/.exec(r)?.[1] ?? "A task");
    const left = [...this.tasks.values()].filter((t) => t.parent === parent).length;
    this.after(400, () => this.begin(parent, results.join("\n\n"), reviewScript(titles, left)));
  }

  private finish(sim: Sim) {
    if (sim.stopped || this.sims.get(sim.thread)?.run !== sim.run) return;
    this.sims.delete(sim.thread);
    const t = this.thread(sim.thread);
    if (!t) return;
    const files = this.filesOf(sim.run);
    const s = stat(files);
    const prev = [...this.records.values()].filter((m) => m.entity.kind === "checkpoint" && m.entity.thread === sim.thread).length;
    const machine = this.machineOf(sim.thread);
    if (prev === 0) this.put("checkpoint", { id: id("chk_"), thread: sim.thread, run: null, ordinal: 0, commit: "f35f34cf", stat: { files: 0, additions: 0, deletions: 0 }, created_at: now() }, machine);
    const run = this.get<Run>("run", sim.run)!;
    const cid = id("chk_");
    this.put("checkpoint", { id: cid, thread: sim.thread, run: sim.run, ordinal: run.ordinal, commit: Math.random().toString(16).slice(2, 18), stat: s, created_at: now() }, machine);
    const usage = { input_tokens: 38_000 + Math.floor(Math.random() * 30_000), output_tokens: 2_000 + Math.floor(Math.random() * 4_000), cached_tokens: 22_000, cost_usd: t.provider === "claude" ? 0.18 + Math.random() * 0.4 : null };
    this.patchRun(sim.run, { status: "completed", completed_at: now(), checkpoint: cid, stat: s, usage });
    const total = {
      input_tokens: t.usage.input_tokens + usage.input_tokens,
      output_tokens: t.usage.output_tokens + usage.output_tokens,
      cached_tokens: t.usage.cached_tokens + usage.cached_tokens,
      cost_usd: usage.cost_usd === null ? t.usage.cost_usd : (t.usage.cost_usd ?? 0) + usage.cost_usd,
    };
    const [next, ...rest] = t.queued;
    this.patchThread(sim.thread, { status: "idle", active_run: null, activity: null, usage: total, queued: rest });
    this.taskDone(sim.thread, sim.run);
    const woken = this.wakes.get(sim.thread);
    if (woken) {
      this.wakes.delete(sim.thread);
      this.wakeOrchestrator(sim.thread, woken);
    } else if (next) this.after(500, () => this.sendMessage(sim.thread, next.text, next.attachments));
  }

  private filesOf(run: string): FileChange[] {
    const out = new Map<string, FileChange>();
    for (const m of this.records.values()) {
      const e = m.entity;
      if (e.kind === "node" && e.run === run && e.body.type === "tool") for (const f of e.body.files) out.set(f.path, f);
    }
    return [...out.values()];
  }

  private stopRun(sim: Sim, status: Run["status"]) {
    this.sims.delete(sim.thread);
    for (const m of [...this.records.values()]) {
      const e = m.entity;
      if (e.kind === "request" && e.run === sim.run && e.status === "pending") {
        const { kind: _k, ...rest } = e;
        this.put("request", { ...rest, status: "cancelled", resolved_at: now() }, m.machine);
      }
      if (e.kind === "node" && e.run === sim.run && (e.status === "running" || e.status === "waiting")) {
        const { kind: _k, ...rest } = e;
        this.putNode({ ...rest, status: "interrupted", completed_at: now() });
      }
    }
    this.patchRun(sim.run, { status, completed_at: now() });
    this.patchThread(sim.thread, { status: "idle", active_run: null, activity: null, pending_requests: 0 });
  }

  private reply(prompt: string, mode: Thread["interaction_mode"] = "default"): Step[] {
    // T3's "Implement": the plan comes back as the message, in build mode.
    if (prompt.startsWith(PLAN_IMPLEMENTATION_PREFIX)) return implementScript(prompt.slice(PLAN_IMPLEMENTATION_PREFIX.length));
    // In plan mode a follow-up refines the plan (T3's "Refine").
    if (mode === "plan") return refineScript(prompt);
    const canned = followUps.default[prompt.length % followUps.default.length];
    return [
      { think: `The user asked: "${prompt.slice(0, 80)}". Look at the code first.` },
      { tool: { name: "Read", kind: "read", title: "README.md", ms: 700 } },
      { tool: { name: "Bash", kind: "command", title: "cargo check", output: "    Checking roll-cli v0.1.0\n    Finished `dev` profile in 4.12s\n", exit: 0 } },
      { say: canned },
    ];
  }

  // ── commands ──

  private handle(machine: MachineId, c: Command): Reply {
    switch (c.type) {
      case "message.send": {
        const t = this.thread(c.thread);
        if (!t) return notFound("thread");
        // A message brings the thread back: off the settled shelf, out of its snooze.
        if (t.inbox.settle || t.inbox.snooze) this.patchInbox(c.thread, { settle: null, snooze: null });
        const busy = t.status !== "idle" && t.status !== "error";
        if (!busy) {
          this.sendMessage(c.thread, c.text, c.attachments ?? []);
          return { type: "ok" };
        }
        // A compaction can't be steered: the message waits behind it (T3).
        if (c.mode === "steer" && !this.sims.get(c.thread)?.compacting) {
          const r = t.active_run && this.get<Run>("run", t.active_run);
          if (r) this.patchRun(r.id, { steers: [...r.steers, c.text] });
          return { type: "ok" };
        }
        const q = { id: id("q_"), text: c.text, attachments: c.attachments ?? [], at: now() };
        this.patchThread(c.thread, { queued: [...t.queued, q] });
        return { type: "created", id: q.id };
      }
      case "message.unqueue": {
        const t = this.thread(c.thread);
        if (!t) return notFound("thread");
        this.patchThread(c.thread, { queued: t.queued.filter((q) => q.id !== c.message) });
        return { type: "ok" };
      }
      case "message.edit": {
        const t = this.thread(c.thread);
        const q = t?.queued.find((m) => m.id === c.message);
        if (!t || !q) return notFound("queued message");
        if (!c.text.trim() && !(c.attachments ?? q.attachments).length) return { type: "error", code: "invalid", message: "the message is empty; remove it from the queue instead" };
        this.patchThread(c.thread, { queued: t.queued.map((m) => (m.id === c.message ? { ...m, text: c.text, attachments: c.attachments ?? m.attachments } : m)) });
        return { type: "ok" };
      }
      case "message.reorder": {
        const t = this.thread(c.thread);
        const q = t?.queued.find((m) => m.id === c.message);
        if (!t || !q || (c.before && !t.queued.some((m) => m.id === c.before))) return notFound("queued message");
        if (c.before === c.message) return { type: "ok" };
        const rest = t.queued.filter((m) => m.id !== c.message);
        const at = c.before ? rest.findIndex((m) => m.id === c.before) : rest.length;
        this.patchThread(c.thread, { queued: [...rest.slice(0, at), q, ...rest.slice(at)] });
        return { type: "ok" };
      }
      case "message.promote": {
        const t = this.thread(c.thread);
        const q = t?.queued.find((m) => m.id === c.message);
        if (!t || !q) return notFound("queued message");
        const sim = this.sims.get(c.thread);
        if (sim?.compacting) return { type: "error", code: "conflict", message: "the conversation is being compacted; the message goes once that's done" };
        this.patchThread(c.thread, { queued: t.queued.filter((m) => m.id !== c.message) });
        if (!sim) {
          // A held queue: this one runs now, and the rest drains after it.
          this.sendMessage(c.thread, q.text, q.attachments);
          return { type: "ok" };
        }
        const r = t.active_run && this.get<Run>("run", t.active_run);
        if (r) this.patchRun(r.id, { steers: [...r.steers, q.text] });
        return { type: "ok" };
      }
      case "run.interrupt": {
        const sim = this.sims.get(c.thread);
        if (sim) {
          sim.stopped = true;
          this.stopRun(sim, "interrupted");
        }
        // Stopping an orchestrator stops what it delegated (T3's Stop); cancelled tasks wake nobody.
        this.wakes.delete(c.thread);
        for (const [child, task] of [...this.tasks]) {
          if (task.parent !== c.thread) continue;
          this.tasks.delete(child);
          this.putNode({ ...task.node, status: "interrupted", completed_at: now() });
          const s = this.sims.get(child);
          if (s) {
            s.stopped = true;
            this.stopRun(s, "interrupted");
          }
        }
        return { type: "ok" };
      }
      case "request.respond": {
        const q = this.get<Request>("request", c.request);
        if (!q || q.status !== "pending") return { type: "error", code: "conflict", message: "the request was already answered" };
        const sim = this.sims.get(q.thread);
        sim?.answer?.(c.response);
        return { type: "ok" };
      }
      case "thread.create": {
        const p = this.get<Project>("project", c.project);
        if (!p) return notFound("project");
        const tid = id("thr_");
        const title = c.title ?? (c.prompt ? c.prompt.split("\n")[0].slice(0, 60) : "New thread");
        this.put(
          "thread",
          {
            id: tid,
            project: c.project,
            title,
            provider: c.provider,
            model: c.model ?? null,
            effort: c.effort ?? null,
            options: c.options ?? {},
            runtime_mode: c.runtime_mode ?? "full_access",
            interaction_mode: c.interaction_mode ?? "default",
            cwd: p.path,
            worktree: c.worktree ? { path: `${p.path}/../.roll-worktrees/${tid}`, branch: `roll/${slug(title)}`, base: "f35f34cf" } : null,
            status: "idle",
            lineage: null,
            orchestrator: c.orchestrator ?? false,
            active_run: null,
            queued: [],
            run_count: 0,
            pending_requests: 0,
            activity: null,
            usage: { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null },
            context: null,
            error: null,
            change: null,
            work: null,
            created_at: now(),
            updated_at: now(),
            archived_at: null,
            inbox: { ...EMPTY_INBOX },
          },
          machine,
        );
        const providers = (this.machines.get(machine)?.info?.providers ?? []).filter((p) => p.installed).map((p) => p.id);
        if (c.prompt) this.after(300, () => this.begin(tid, c.prompt!, c.orchestrator ? orchestratorScript(c.prompt!, providers) : this.reply(c.prompt!, c.interaction_mode), c.attachments ?? []));
        return { type: "created", id: tid };
      }
      case "thread.configure": {
        const { type: _t, thread, ...patch } = c;
        if (!this.thread(thread)) return notFound("thread");
        this.patchThread(thread, Object.fromEntries(Object.entries(patch).filter(([, v]) => v !== undefined)));
        return { type: "ok" };
      }
      case "thread.fork": {
        const t = this.thread(c.thread);
        if (!t) return notFound("thread");
        const tid = id("thr_");
        const { kind: _k, ...rest } = t as Thread & { kind?: string };
        this.put(
          "thread",
          {
            ...rest,
            id: tid,
            title: `${t.title} (fork)`,
            status: "idle",
            active_run: null,
            queued: [],
            pending_requests: 0,
            activity: null,
            run_count: 0,
            usage: { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null },
            // The parent's context at the fork point, until the fork's own conversation reports.
            context: (c.at ? this.get<Run>("run", c.at)?.context : t.context) ?? null,
            lineage: { parent: t.id, relation: "fork", root: t.lineage?.root ?? t.id, from_run: c.at ?? t.active_run ?? null },
            created_at: now(),
            updated_at: now(),
          },
          machine,
        );
        return { type: "created", id: tid };
      }
      case "thread.archive":
        this.patchThread(c.thread, { archived_at: now() });
        return { type: "ok" };
      case "thread.settle": {
        const t = this.thread(c.thread);
        if (!t) return notFound("thread");
        const refused = this.parkable(t, "settling", true);
        if (refused) return refused;
        this.patchInbox(c.thread, { settle: t.inbox.settle?.state === "settled" ? t.inbox.settle : { state: "settled", at: now(), by: "user" }, snooze: null });
        return { type: "ok" };
      }
      case "thread.unsettle":
        if (!this.thread(c.thread)) return notFound("thread");
        this.patchInbox(c.thread, { settle: { state: "active", at: now() } });
        return { type: "ok" };
      case "thread.snooze": {
        const t = this.thread(c.thread);
        if (!t) return notFound("thread");
        if (c.until <= now()) return { type: "error", code: "invalid", message: "choose a time in the future to snooze until" };
        const refused = this.parkable(t, "snoozing", false);
        if (refused) return refused;
        this.patchInbox(c.thread, { snooze: { until: c.until, at: now() }, settle: t.inbox.settle?.state === "settled" ? null : t.inbox.settle });
        this.wakeAt(c.thread, c.until);
        return { type: "ok" };
      }
      case "thread.unsnooze":
        if (!this.thread(c.thread)) return notFound("thread");
        this.patchInbox(c.thread, { snooze: null });
        return { type: "ok" };
      case "thread.visit":
        if (!this.thread(c.thread)) return notFound("thread");
        this.patchInbox(c.thread, { visited_at: now() }, false);
        return { type: "ok" };
      case "thread.auto_settle":
        if (!this.thread(c.thread)) return notFound("thread");
        this.patchInbox(c.thread, { manual: !c.enabled });
        return { type: "ok" };
      case "thread.change_ended": {
        const t = this.thread(c.thread);
        if (!t) return notFound("thread");
        if (!t.inbox.settle && !t.inbox.manual && !t.inbox.snooze && !t.archived_at && !this.parkable(t, "settling", true))
          this.patchInbox(c.thread, { settle: { state: "settled", at: t.updated_at, by: c.landed ? "landed" : "closed" } });
        return { type: "ok" };
      }
      case "thread.unarchive":
        this.patchThread(c.thread, { archived_at: null });
        return { type: "ok" };
      case "thread.delete": {
        const sim = this.sims.get(c.thread);
        if (sim) sim.stopped = true;
        for (const m of [...this.records.values()]) {
          const e = m.entity;
          if (e.kind !== "removed" && e.kind !== "project" && e.kind !== "thread" && "thread" in e && e.thread === c.thread) this.remove(e.kind, e.id, c.thread);
        }
        this.remove("thread", c.thread, c.thread);
        return { type: "ok" };
      }
      case "checkpoint.rollback": {
        const cp = this.get<Checkpoint>("checkpoint", c.checkpoint);
        if (!cp) return notFound("checkpoint");
        const t = this.thread(c.thread);
        if (t && t.status !== "idle") return { type: "error", code: "conflict", message: "the thread is running" };
        for (const m of this.records.values()) {
          const e = m.entity;
          if (e.kind === "run" && e.thread === c.thread && e.ordinal > cp.ordinal && e.status !== "rolled_back") this.patchRun(e.id, { status: "rolled_back" });
        }
        return { type: "ok" };
      }
      case "thread.publish": {
        const t = this.thread(c.thread);
        const repo = t && this.get<Project>("project", t.project)?.repo;
        if (!repo) return { type: "published", branch: t?.worktree?.branch ?? "roll/demo", commit: "4b825dc6", change: null, url: null, landing: null };
        const number = t.change?.number ?? 42 + (seq % 50);
        const url = `/${repo.owner}/${repo.repo}/changes/${number}`;
        const change = (status: ChangeStatus, detail: string | null) =>
          this.patchThread(c.thread, { change: { repo: `${repo.owner}/${repo.repo}`, number, status, url, detail } });
        change("checking", "2 of 3 checks running");
        if (c.land) {
          this.after(2500, () => change("queued", "1st in the merge queue"));
          // Landed: the relay tells the daemon, which settles the thread.
          this.after(6000, () => {
            change("landed", null);
            this.handle(machine, { type: "thread.change_ended", thread: c.thread, landed: true });
          });
        } else this.after(4000, () => change("ready", "All 3 checks passed"));
        const landing: Landing | null = c.land ? { status: "queued", message: "Queued: it lands once its batch passes the checks.", blockers: [] } : null;
        return { type: "published", branch: t.worktree?.branch ?? `roll/${slug(t.title)}`, commit: "4b825dc6", change: number, url, landing };
      }
      case "diff.get": {
        const runs = c.run ? [c.run] : [...this.records.values()].filter((m) => m.entity.kind === "run" && m.entity.thread === c.thread).map((m) => m.entity.id);
        const files = runs.flatMap((r) => this.filesOf(r));
        return { type: "diff", stat: stat(files), files, truncated: false };
      }
      case "git.status": {
        const t = this.thread(c.thread);
        const files = [...this.records.values()].filter((m) => m.entity.kind === "run" && m.entity.thread === c.thread).flatMap((m) => this.filesOf(m.entity.id));
        return { type: "git_status", branch: t?.worktree?.branch ?? "main", ahead: 0, behind: 0, files };
      }
      case "fs.list":
        return { type: "entries", entries: demoDir(c.path) };
      case "fs.read":
        return { type: "file", path: c.path, text: "# roll\n\nkeep the code rolling\n", size: 27 };
      case "project.add": {
        const pid = id("prj_");
        const name = c.name ?? c.path.split("/").filter(Boolean).pop() ?? "project";
        this.put(
          "project",
          {
            id: pid,
            name,
            path: c.path,
            repo: c.clone ?? null,
            git: { branch: "main", head: "f35f34cf", remote: null, dirty: false },
            settings: { ...NO_SETTINGS },
            pull: null,
            logo: null,
            created_at: now(),
            updated_at: now(),
          },
          machine,
        );
        return { type: "created", id: pid };
      }
      case "project.remove":
        this.remove("project", c.project, null);
        return { type: "ok" };
      case "project.update": {
        const p = this.get<Project>("project", c.project);
        if (!p) return notFound("project");
        // The icon checked as the daemon does; a `file` icon keeps the logo it has (the demo has no files to read).
        const checked = c.settings?.icon ? checkProjectIcon(c.settings.icon) : null;
        if (checked && "error" in checked) return { type: "error", code: "invalid", message: checked.error };
        const icon = checked && checked.icon.kind !== "auto" ? checked.icon : undefined;
        const settings = c.settings
          ? { auto_pull: c.settings.auto_pull, defaults: c.settings.defaults ?? {}, scripts: c.settings.scripts ?? [], auto_settle: c.settings.auto_settle ?? DEFAULT_AUTO_SETTLE, color: c.settings.color, icon }
          : p.settings;
        const next = { ...p, name: c.name?.trim() || p.name, settings, updated_at: now() };
        this.put("project", next, machine);
        if (settings.auto_pull && !p.settings.auto_pull) this.after(600, () => this.demoPull(c.project, machine));
        return { type: "ok" };
      }
      case "project.pull": {
        const result = this.demoPull(c.project, machine);
        return result ? { type: "pulled", ...result } : notFound("project");
      }
      case "terminal.open": {
        const tid = id("trm_");
        const p = c.project ? this.get<Project>("project", c.project) : undefined;
        const th = c.thread ? this.thread(c.thread) : undefined;
        const cwd = th?.cwd ?? p?.path ?? this.machines.get(machine)?.info?.home ?? "/";
        const term: Terminal = { id: tid, project: c.project ?? null, thread: c.thread ?? null, cwd, title: c.command ?? "zsh", status: "running", exit_code: null, cols: c.cols, rows: c.rows, created_at: now() };
        this.put("terminal", term, machine);
        const out = (s: string) => this.emit({ type: "delta", machine, delta: { type: "terminal", terminal: tid, data: b64(s) } });
        this.after(200, () => {
          if (c.command) {
            out(`\x1b[2m$ ${c.command}\x1b[0m\r\n`);
            out("Opening the browser to log in…\r\nIf it doesn't open, visit \x1b[4mhttps://claude.ai/oauth/device\x1b[0m and enter \x1b[1mWXYZ-2345\x1b[0m\r\n");
          } else out(`\x1b[2mroll demo shell · ${cwd}\x1b[0m\r\n$ `);
        });
        return { type: "created", id: tid };
      }
      case "terminal.input": {
        const m = this.records.get(`terminal:${c.terminal}`);
        if (!m) return notFound("terminal");
        const text = atobSafe(c.data).replace(/\r/g, "\r\n$ ");
        this.emit({ type: "delta", machine: m.machine, delta: { type: "terminal", terminal: c.terminal, data: b64(text) } });
        return { type: "ok" };
      }
      case "terminal.resize":
        return { type: "ok" };
      case "terminal.close":
        this.remove("terminal", c.terminal, null);
        return { type: "ok" };
      case "terminal.scrollback":
        return { type: "scrollback", data: "" };
      case "provider.refresh": {
        const m = this.machines.get(machine);
        if (m?.info) {
          const all = (this.allProviders.get(machine) ?? m.info.providers).map((p) => (p.auth === "missing" ? { ...p, auth: "ready" as const, detail: "Logged in" } : p));
          this.allProviders.set(machine, all);
          const set = m.info.settings?.set ?? {};
          const next = { ...m, info: { ...m.info, providers: offeredProviders(all, set) } };
          this.machines.set(machine, next);
          this.emit({ type: "machine", cursor: ++this.cursor, machine: next });
        }
        return { type: "ok" };
      }
      case "machine.configure": {
        const m = this.machines.get(machine);
        if (!m?.info) return notFound("machine");
        const error = settingsError(c.settings);
        if (error) return { type: "error", code: "invalid", message: error };
        const all = this.allProviders.get(machine) ?? m.info.providers;
        const fileEnv = Object.fromEntries((m.info.settings?.providers ?? []).map((p) => [p.id, p.env]));
        const next = { ...m, info: { ...m.info, ...demoConfig(m.info.home, all, c.settings, fileEnv) } };
        this.machines.set(machine, next);
        this.emit({ type: "machine", cursor: ++this.cursor, machine: next });
        return { type: "ok" };
      }
    }
  }
}

const notFound = (what: string): Reply => ({ type: "error", code: "not_found", message: `no such ${what}` });

/** An SVG as a `data:` URL, as the daemon sends a project's logo. */
const svgUrl = (svg: string) => `data:image/svg+xml;base64,${btoa(svg)}`;

const slug = (s: string) =>
  s
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 40);

function b64(s: string) {
  const bytes = new TextEncoder().encode(s);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

function atobSafe(s: string) {
  try {
    const bin = atob(s);
    return new TextDecoder().decode(Uint8Array.from(bin, (c) => c.charCodeAt(0)));
  } catch {
    return "";
  }
}

function demoDir(path: string): FsEntry[] {
  const dirs: Record<string, string[]> = {
    "/Users/ada": ["roll", "web", "notes", "scratch", ".config"],
    "/home/roll": ["infra", "cache"],
  };
  const names = dirs[path.replace(/\/$/, "")] ?? ["src", "docs", "tests"];
  return [...names.map((name) => ({ name, dir: true, size: 0 })), { name: "README.md", dir: false, size: 1204 }];
}

