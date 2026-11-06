/**
 * The Gateway UI (docs/code/gateway.md): setup's five steps under a macOS click (Safari), keys
 * sealed before anything is sent, THE NOTICE wherever a machine uses the gateway, the members'
 * read-only endpoint, the live request log, and the usage charts.
 */
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Command, EndpointInfo, EndpointSpec, GatewayPlan, LogEntry, Machine, Reply, UsageReport } from "@/lib/agents-api";
import type { GatewayApi, GatewayCommands } from "@/lib/agents-gateway";
import { machine, machineConfig, provider, SEAL_KEY } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { macClick } from "@/test/mac-click";
import { MachineEndpoints, profileFor } from "./endpoint-settings";
import { RequestLog, TAIL_MS } from "./gateway/log";
import { MembersList } from "./gateway/members";
import { NewSandboxGatewayNotice, ProviderGatewayNote } from "./gateway/machine";
import { UsageCharts } from "./gateway/usage";
import { GatewaySetup } from "./gateway/wizard";

const HOST = machine({ id: "mch_srv", name: "ai-server", info: { hostname: "ai-server", os: "linux", arch: "x86_64", version: "0.1.0", home: "/home/ada", providers: [provider()], settings: machineConfig(), seal_key: SEAL_KEY } });
const LAPTOP = machine({ id: "mch_mbp", name: "ada-mbp", info: { hostname: "ada-mbp", os: "macos", arch: "aarch64", version: "0.1.0", home: "/Users/ada", providers: [provider()], settings: machineConfig(), seal_key: SEAL_KEY } });

function memoryGateway(seed: GatewayPlan | null = null) {
  let plan = seed;
  const api: GatewayApi = {
    get: vi.fn(async () => plan),
    save: vi.fn(async (_o, p) => (plan = { ...p, members: plan?.members ?? [], updated_at: 1 })),
    remove: vi.fn(async () => void (plan = null)),
    join: vi.fn(async (_o, j) => {
      plan = { ...plan!, members: [...(plan!.members ?? []), { machine: j.machine, url: j.url ?? "", client: `gwc_${j.machine}`, joined_at: 1 }] };
    }),
    leave: vi.fn(async (_o, m) => {
      plan = { ...plan!, members: (plan!.members ?? []).filter((x) => x.machine !== m) };
    }),
  };
  return { api, plan: () => plan };
}

describe("setting up the gateway", () => {
  it("picks an always-on computer, not the laptop listed first", () => {
    const h = agentsHarness({ machines: [LAPTOP, HOST], ctx: { services: fakeServices({ gateway: memoryGateway().api }) } });
    renderAgents(<GatewaySetup machines={[LAPTOP, HOST]} onClose={vi.fn()} onDone={vi.fn()} />, h);
    const dlg = screen.getByRole("dialog", { name: "Set up the model gateway" });
    expect(within(dlg).getByRole("radio", { name: "ai-server" })).toBeChecked();
    expect(within(dlg).getByRole("radio", { name: "ada-mbp" })).not.toBeChecked();
  });

  it("takes focus (the shared dialog keeps it inside); Escape closes an open upstream editor first, then the wizard (M5)", async () => {
    const h = agentsHarness({ machines: [LAPTOP, HOST], ctx: { services: fakeServices({ gateway: memoryGateway().api }) } });
    const onClose = vi.fn();
    renderAgents(<GatewaySetup machines={[LAPTOP, HOST]} initialHost="mch_srv" onClose={onClose} onDone={vi.fn()} />, h);
    const dlg = screen.getByRole("dialog", { name: "Set up the model gateway" });
    await waitFor(() => expect(dlg.contains(document.activeElement)).toBe(true));
    macClick(within(dlg).getByRole("button", { name: "Continue" }));
    macClick(within(dlg).getByRole("checkbox", { name: /I understand/ }));
    macClick(within(dlg).getByRole("button", { name: "Continue" }));
    macClick(within(dlg).getByRole("button", { name: "Add an upstream" }));
    expect(within(dlg).getByLabelText("API key")).toBeInTheDocument();
    fireEvent.keyDown(within(dlg).getByLabelText("API key"), { key: "Escape" });
    await waitFor(() => expect(within(dlg).queryByLabelText("API key")).toBeNull());
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(dlg, { key: "Escape" });
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("walks the five steps under a macOS click, seals the key before sending it, and acknowledges the notice", async () => {
    const gw = memoryGateway();
    const h = agentsHarness({
      machines: [HOST, LAPTOP],
      reply: (c: Command): Reply => (c.type === "gateway.test" ? { type: "gateway_tested", checks: [{ upstream: "anthropic", ok: true, ms: 120, models: 9 }] } : { type: "ok" }),
      ctx: { services: fakeServices({ gateway: gw.api }) },
    });
    const done = vi.fn();
    const onClose = vi.fn();
    renderAgents(<GatewaySetup machines={[LAPTOP, HOST]} initialHost="mch_srv" onClose={onClose} onDone={done} />, h);
    const dlg = screen.getByRole("dialog", { name: "Set up the model gateway" });
    const next = () => macClick(within(dlg).getByRole("button", { name: /Continue|Review/ }));

    // 1. The computer: ai-server, picked.
    expect(within(dlg).getByRole("radio", { name: "ai-server" })).toBeChecked();
    next();
    // 2. The notice, and Continue waits for the acknowledgement.
    expect(dlg.textContent).toContain("All AI requests from these machines go out from ai-server. Providers see ai-server's network and IP address, not these machines'. If ai-server is offline, requests from these machines fail until it's back.");
    expect(within(dlg).getByRole("button", { name: "Continue" })).toBeDisabled();
    macClick(within(dlg).getByRole("checkbox", { name: /I understand/ }));
    expect(onClose).not.toHaveBeenCalled();
    next();
    // 3. An upstream with a key: sealed at once, the field emptied.
    macClick(within(dlg).getByRole("button", { name: "Add an upstream" }));
    fireEvent.change(within(dlg).getByLabelText("API key"), { target: { value: "sk-ant-plaintext-42" } });
    macClick(within(dlg).getByRole("button", { name: "Add upstream" }));
    await waitFor(() => expect(within(dlg).getByText("key sealed")).toBeInTheDocument());
    next();
    // 4. An address for the laptop, the laptop picked, fallback on: the notice names it.
    fireEvent.change(within(dlg).getByLabelText("Address"), { target: { value: "ai-server.tail.ts.net" } });
    fireEvent.change(within(dlg).getByLabelText("Its IP address"), { target: { value: [100, 64, 0, 9].join(".") } });
    macClick(within(dlg).getByRole("button", { name: "Add" }));
    macClick(within(dlg).getByRole("checkbox", { name: "ada-mbp" }));
    macClick(within(dlg).getByRole("switch", { name: "Fall back to their own logins" }));
    expect(dlg.textContent).toContain("All AI requests from ada-mbp go out from ai-server. Providers see ai-server's network and IP address, not ada-mbp's. If ai-server is offline, requests from ada-mbp fail, and its agents use their own logins or keys instead.");
    next();
    // 5. Set up and test.
    macClick(within(dlg).getByRole("button", { name: "Set up and test" }));
    await waitFor(() => expect(within(dlg).getByRole("button", { name: "Done" })).toBeInTheDocument());
    expect(within(dlg).getByText("9 models · 120 ms")).toBeInTheDocument();

    const sent = h.command.mock.calls.map(([m, , c]) => [m, c] as [string, Command]);
    expect(sent.map(([m, c]) => `${m} ${c.type}`)).toEqual(["mch_srv gateway.configure", "mch_srv gateway.key", "mch_srv gateway.test"]);
    const configure = sent[0][1] as Extract<Command, { type: "gateway.configure" }>;
    expect(configure.settings).toEqual({ enabled: true, port: 8790, listen: [[100, 64, 0, 9].join(".")], upstreams: [{ id: "anthropic", name: "Anthropic", kind: "anthropic" }] });
    expect(sent[1][1]).toMatchObject({ type: "gateway.key", upstream: "anthropic", key: { type: "set", sealed: { v: 1 } } });
    // The key never left the browser in the clear: not in a command, not to the relay.
    const everything = JSON.stringify([sent, (gw.api.save as ReturnType<typeof vi.fn>).mock.calls, (gw.api.join as ReturnType<typeof vi.fn>).mock.calls]);
    expect(everything).not.toContain("sk-ant-plaintext");
    expect(gw.plan()).toMatchObject({ host: "mch_srv", fallback: true, urls: [{ label: "Tailscale", url: "http://ai-server.tail.ts.net:8790" }] });
    expect(gw.plan()!.acknowledged_at).toBeGreaterThan(0);
    expect((gw.api.join as ReturnType<typeof vi.fn>).mock.calls.map(([, j]) => j)).toEqual([
      { machine: "mch_mbp", url: "http://ai-server.tail.ts.net:8790" },
      { machine: "mch_srv", url: "http://127.0.0.1:8790" },
    ]);
    expect(done).toHaveBeenCalled();
  });

  it("won't run on a sandbox or a computer that can't take sealed keys", () => {
    const old = machine({ id: "mch_old", name: "old-box", info: { ...HOST.info!, seal_key: undefined } });
    const sbx = machine({ id: "mch_sbx", name: "sbx", sandbox: { backend: "docker", size: "s", status: "running", hourly_cents: 0 } });
    const h = agentsHarness({ machines: [old, sbx], ctx: { services: fakeServices({ gateway: memoryGateway().api }) } });
    renderAgents(<GatewaySetup machines={[old, sbx]} onClose={() => {}} onDone={() => {}} />, h);
    expect(screen.getByRole("radio", { name: "old-box" })).toBeDisabled();
    expect(screen.getByText("Update roll on it first.")).toBeInTheDocument();
    expect(screen.queryByRole("radio", { name: "sbx" })).toBeNull();
    expect(screen.getByRole("button", { name: "Continue" })).toBeDisabled();
  });
});

const PLAN: GatewayPlan = {
  host: "mch_srv",
  urls: [{ label: "Tailscale", url: "http://ai-server.tail.ts.net:8790" }],
  members: [
    { machine: "mch_srv", url: "http://127.0.0.1:8790", client: "gwc_1", joined_at: 1 },
    { machine: "mch_mbp", url: "http://ai-server.tail.ts.net:8790", client: "gwc_2", joined_at: 1, error: "the machine isn't connected" },
  ],
  new_sandboxes: true,
  fallback: false,
  acknowledged_at: 1,
  updated_at: 1,
};

describe("the notice, wherever a machine uses the gateway", () => {
  const endpoint: EndpointInfo = { id: "roll-gateway", name: "roll gateway on ai-server", kind: "gateway", base_url: "http://ai-server.tail.ts.net:8790", providers: ["claude"], key: "sealed", models: [{ id: "fast" }] };
  const member: Machine = { ...LAPTOP, info: { ...LAPTOP.info!, settings: machineConfig({}, { endpoints: [endpoint] }) } };

  it("in a member's endpoints: read-only, managed, with the notice", () => {
    const h = agentsHarness({ machines: [member], ctx: { services: fakeServices() } });
    renderAgents(<MachineEndpoints machine={member} editing={null} onEdit={() => {}} />, h);
    const row = screen.getByRole("group", { name: "Endpoint roll gateway on ai-server" });
    expect(row.textContent).toContain("All AI requests from this machine go out from ai-server. Providers see ai-server's network and IP address, not this machine's. If ai-server is offline, requests from this machine fail until it's back.");
    expect(within(row).getByText("managed")).toBeInTheDocument();
    expect(within(row).queryByRole("button", { name: /Edit|Remove/ })).toBeNull();
    expect(within(row).getByRole("button", { name: /Manage in Gateway settings/ })).toBeInTheDocument();
  });

  it("can't be undone from another endpoint: the editor and Apply to machines leave the gateway's providers alone", () => {
    const h = agentsHarness({ machines: [member], ctx: { services: fakeServices() } });
    renderAgents(<MachineEndpoints machine={member} editing={{}} onEdit={() => {}} />, h);
    const claude = screen.getByRole("checkbox", { name: /Claude Code/ });
    expect(claude).toBeDisabled();
    expect(claude.closest("label")).toHaveTextContent("Managed in Gateway settings");
    const profile = { id: "proxy", name: "Proxy", kind: "anthropic", base_url: "http://proxy:8317", providers: ["claude"] } as EndpointSpec;
    expect(profileFor(profile, member).providers ?? []).toEqual([]);
  });

  it("under a provider that goes through it, with fallback", () => {
    const fb: Machine = { ...member, info: { ...member.info!, settings: machineConfig({}, { endpoints: [{ ...endpoint, fallback: true }] }) } };
    const h = agentsHarness({ machines: [fb] });
    renderAgents(<ProviderGatewayNote machine={fb} provider="claude" />, h);
    expect(document.body.textContent).toContain("If ai-server is offline, requests from this machine fail, and its agents use their own logins or keys instead.");
  });

  it("over the Gateway section's machines, with their names and why one failed to join", async () => {
    const gw = memoryGateway(PLAN);
    const h = agentsHarness({ machines: [HOST, LAPTOP], ctx: { services: fakeServices({ gateway: gw.api }) } });
    renderAgents(<MembersList plan={PLAN} host={HOST} port={8790} machines={[HOST, LAPTOP]} seen={[{ client: "gwc_1", at: Date.now() }]} onChange={async () => {}} />, h);
    expect(document.body.textContent).toContain("All AI requests from ada-mbp go out from ai-server. Providers see ai-server's network and IP address, not ada-mbp's. If ai-server is offline, requests from ada-mbp fail until it's back.");
    expect(screen.getByText("the machine isn't connected")).toBeInTheDocument();
    // Leaving asks first, then the relay revokes.
    macClick(screen.getByRole("button", { name: "ada-mbp stops using the gateway" }));
    macClick(screen.getByRole("button", { name: "Stop using it" }));
    await waitFor(() => expect(gw.api.leave).toHaveBeenCalledWith("acme", "mch_mbp"));
  });

  it("in the new-sandbox dialog when new sandboxes join", async () => {
    const h = agentsHarness({ machines: [HOST], ctx: { services: fakeServices({ gateway: memoryGateway(PLAN).api }) } });
    renderAgents(<NewSandboxGatewayNotice backend="docker" />, h);
    await waitFor(() =>
      expect(document.body.textContent).toContain("All AI requests from new sandboxes go out from ai-server. Providers see ai-server's network and IP address, not the sandboxes'. If ai-server is offline, requests from new sandboxes fail until it's back."),
    );
  });

  it("in the new-sandbox dialog, says a sandbox roll hosts can't reach it (and won't join)", async () => {
    const h = agentsHarness({ machines: [HOST], ctx: { services: fakeServices({ gateway: memoryGateway(PLAN).api }) } });
    renderAgents(<NewSandboxGatewayNotice backend="agent37" />, h);
    await waitFor(() => expect(document.body.textContent).toContain("Sandboxes roll hosts can't reach your gateway on ai-server: this one uses its own keys or login."));
    expect(document.body.textContent).not.toContain("All AI requests from new sandboxes");
  });

  it("never offers a hosted sandbox as a member: it's listed as unreachable and can't be added", async () => {
    const hosted: Machine = { ...LAPTOP, id: "mch_box", name: "build-box", sandbox: { backend: "agent37", size: "small", status: "running", hourly_cents: 3 } as Machine["sandbox"] };
    const gw = memoryGateway(PLAN);
    const h = agentsHarness({ machines: [HOST, hosted], ctx: { services: fakeServices({ gateway: gw.api }) } });
    renderAgents(<MembersList plan={{ ...PLAN, members: [] }} host={HOST} port={8790} machines={[HOST, hosted]} seen={[]} onChange={async () => {}} />, h);
    macClick(screen.getByRole("combobox", { name: "A machine to add" }));
    expect(await screen.findByRole("option", { name: /build-box/ })).toHaveAttribute("aria-disabled", "true");
  });
});

const entry = (seq: number, over: Partial<LogEntry> = {}): LogEntry => ({
  seq,
  at: Date.now() - (100 - seq) * 1000,
  machine: "mch_mbp",
  format: "messages",
  model: "claude-sonnet-4-6",
  stream: true,
  status: 200,
  ms: 900,
  attempts: [{ upstream: "anthropic", model: "claude-sonnet-4-6", status: 200, ms: 640 }],
  input_tokens: 1000,
  output_tokens: 100,
  cache_read_tokens: 0,
  cache_write_tokens: 0,
  cost_micros: 4500,
  ...over,
});

function fakeCommands(over: Partial<GatewayCommands> = {}): GatewayCommands {
  return {
    configure: vi.fn(),
    setKey: vi.fn(),
    test: vi.fn(),
    status: vi.fn(),
    usage: vi.fn(),
    log: vi.fn(async () => ({ type: "gateway_log" as const, entries: [] })),
    body: vi.fn(),
    revoke: vi.fn(),
    remove: vi.fn(),
    ...over,
  } as GatewayCommands;
}

describe("the request log", () => {
  it("tails what's new every 2 s, shows a failover's chain and its bodies, and filters errors", async () => {
    let next = 3;
    const log = vi.fn(async (q: { after?: number; errors?: boolean }) => {
      if (q.errors) return { type: "gateway_log" as const, entries: [entry(2, { attempts: [{ upstream: "anthropic", model: "claude-sonnet-4-6", status: 529, ms: 90, error: "overloaded" }, { upstream: "bedrock", model: "us.anthropic.claude-sonnet-4-6", status: 200, ms: 800 }], has_body: true })] };
      if (q.after === undefined) return { type: "gateway_log" as const, entries: [entry(2, { attempts: [{ upstream: "anthropic", model: "claude-sonnet-4-6", status: 529, ms: 90, error: "overloaded" }, { upstream: "bedrock", model: "us.anthropic.claude-sonnet-4-6", status: 200, ms: 800 }], has_body: true }), entry(1)] };
      return { type: "gateway_log" as const, entries: q.after < next ? [entry(next++, { model: "fast" })] : [] };
    });
    const body = vi.fn(async () => ({ type: "gateway_log_body" as const, seq: 2, request: '{"model":"claude-sonnet-4-6"}', response: "event: message_start" }));
    const h = agentsHarness({ machines: [HOST, LAPTOP] });
    renderAgents(<RequestLog gw={fakeCommands({ log, body })} logBodies upstreamName={(id) => (id === "bedrock" ? "Amazon Bedrock" : "Anthropic")} machineName={(id) => (id === "mch_mbp" ? "ada-mbp" : "?")} />, h);
    await waitFor(() => expect(screen.getAllByRole("listitem", { name: /^Request / })).toHaveLength(2));
    expect(log).toHaveBeenCalledWith({ errors: false, limit: 100 });
    // The tail asks for what's after the newest it has.
    await waitFor(() => expect(screen.getAllByRole("listitem", { name: /^Request / })).toHaveLength(3), { timeout: TAIL_MS * 2 });
    expect(log).toHaveBeenCalledWith({ after: 2, errors: false, limit: 100 });
    expect(screen.getAllByRole("listitem", { name: /^Request / })[0]).toHaveAccessibleName("Request 3");

    const failover = screen.getByRole("listitem", { name: "Request 2" });
    expect(within(failover).getByText("failover")).toBeInTheDocument();
    macClick(within(failover).getAllByRole("button")[0]);
    const chain = within(failover).getByRole("list", { name: "Attempts" });
    expect(chain.textContent).toMatch(/Anthropic.*529.*overloaded.*Amazon Bedrock.*200/);
    macClick(within(failover).getByRole("button", { name: "Show the bodies" }));
    await waitFor(() => expect(within(failover).getByText("event: message_start")).toBeInTheDocument());
    expect(body).toHaveBeenCalledWith(2);

    macClick(screen.getByRole("switch", { name: "Only errors and failovers" }));
    await waitFor(() => expect(screen.getAllByRole("listitem", { name: /^Request / })).toHaveLength(1));
    expect(log).toHaveBeenCalledWith({ errors: true, limit: 100 });
  });
});

describe("usage and cost", () => {
  const DAY = 86_400_000;
  it("draws a week by day with every group's total, a table, and a day's values on focus", async () => {
    const today = Math.ceil(Date.now() / DAY) * DAY;
    const rows = [0, 1, 2].flatMap((d) => [
      { at: today - (d + 1) * DAY, key: "claude-opus-5-5", requests: 10, errors: 1, input_tokens: 1000, output_tokens: 100, cache_read_tokens: 0, cache_write_tokens: 0, cost_micros: 2_000_000 },
      { at: today - (d + 1) * DAY, key: "fast", requests: 30, errors: 0, input_tokens: 500, output_tokens: 50, cache_read_tokens: 0, cache_write_tokens: 0, cost_micros: 250_000 },
    ]);
    const usage = vi.fn(async (from: number, to: number, by: UsageReport["by"]) => ({ type: "gateway_usage" as const, by, bucket: "day" as const, from, to, rows: by === "model" ? rows : [] }));
    const h = agentsHarness({ machines: [HOST] });
    renderAgents(<UsageCharts gw={fakeCommands({ usage })} upstreamName={(id) => id} />, h);
    const legend = await screen.findByRole("list", { name: "Legend" });
    expect(legend.textContent).toContain("claude-opus-5-5$6.00");
    expect(legend.textContent).toContain("fast$0.75");
    const totals = screen.getByLabelText("Totals");
    expect(totals.textContent).toContain("$6.75");
    expect(totals.textContent).toContain("120");
    expect(screen.getByRole("img", { name: /Usage by day: \$6\.75 over 7 days/ })).toBeInTheDocument();
    expect(usage.mock.calls[0][1] - usage.mock.calls[0][0]).toBe(7 * DAY);

    const day = screen.getAllByRole("button", { name: /: \$2\.25$/ })[0];
    act(() => day.focus());
    const tip = screen.getByRole("tooltip");
    expect(tip.textContent).toContain("$2.00claude-opus-5-5");
    expect(tip.textContent).toContain("$2.25in all");

    macClick(screen.getByRole("button", { name: "Show as a table" }));
    expect(screen.getByRole("table")).toBeInTheDocument();
    expect(screen.getAllByRole("row")).toHaveLength(8);

    macClick(screen.getByRole("button", { name: "Requests" }));
    await waitFor(() => expect(screen.getByRole("list", { name: "Legend" }).textContent).toContain("fast90"));
  });
});
