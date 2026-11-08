// roll's Pi extension, loaded with `pi --mode rpc --extension <this file>` by roll's Pi adapter
// (crates/agent-host/src/provider/pi.rs). Written for roll after T3 Code's injected extension
// (MIT, packages/provider-pi/src/server/mcpExtensionSource.ts).
//
// - Approvals: Pi leaves permission policy to extensions, so a `tool_call` hook keeps roll's
//   runtime modes: `full_access` asks nothing; read-only tools (and MCP tools marked
//   `readOnlyHint`) never ask; `auto_edit` lets `edit` and `write` through; anything else asks
//   with `ctx.ui.confirm("Allow <tool>?", <input as JSON>)`, which reaches roll as an
//   `extension_ui_request`.
// - MCP: Pi has no MCP client, so this one speaks streamable HTTP to each server in
//   ROLL_PI_MCP and registers its tools as `mcp__<server>__<tool>`.
// - Instructions: ROLL_PI_INSTRUCTIONS joins the system prompt before each agent run.
//
// Everything secret (MCP headers) arrives in the environment; nothing here is written to disk.
// No roll imports: Pi resolves `typebox` and its own types from the user's install.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const PROTOCOL = "2025-06-18";
const READ_ONLY_TOOLS = new Set(["read", "grep", "find", "ls"]);
const FILE_CHANGE_TOOLS = new Set(["edit", "write"]);
const CONNECT_TIMEOUT_MS = 10_000;
const MAX_TOOL_PAGES = 50;

type Mode = "full_access" | "auto_edit" | "ask";
type Server = { name: string; url: string; headers: Record<string, string> };
type McpTool = {
  name: string;
  description?: string;
  inputSchema?: Record<string, unknown>;
  annotations?: { readOnlyHint?: boolean };
};
type JsonRpcReply = { id?: number | string; result?: unknown; error?: { message?: string } };

function env(name: string): string | undefined {
  const value = process.env[name];
  return value && value.length > 0 ? value : undefined;
}

function mode(): Mode {
  const value = env("ROLL_PI_RUNTIME_MODE");
  return value === "auto_edit" || value === "ask" ? value : "full_access";
}

function servers(): Server[] {
  const raw = env("ROLL_PI_MCP");
  if (raw === undefined) return [];
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (s): s is Server =>
        typeof s === "object" && s !== null && typeof s.name === "string" && typeof s.url === "string",
    ).map((s) => ({ name: s.name, url: s.url, headers: typeof s.headers === "object" && s.headers !== null ? s.headers : {} }));
  } catch {
    return [];
  }
}

function summary(input: unknown): string {
  try {
    return (JSON.stringify(input, null, 2) ?? "").slice(0, 4_000);
  } catch {
    return String(input).slice(0, 4_000);
  }
}

function parseReply(body: string, contentType: string): JsonRpcReply {
  if (contentType.includes("text/event-stream")) {
    for (const line of body.split("\n")) {
      const data = line.startsWith("data:") ? line.slice(5).trim() : "";
      if (data.length === 0) continue;
      const parsed = JSON.parse(data) as JsonRpcReply;
      if (parsed.id !== undefined || parsed.result !== undefined || parsed.error !== undefined) return parsed;
    }
    throw new Error("the MCP server's event stream had no JSON-RPC reply");
  }
  return JSON.parse(body) as JsonRpcReply;
}

function schema(input: Record<string, unknown> | undefined) {
  const unsafe = (Type as { Unsafe?: (value: unknown) => unknown }).Unsafe;
  if (typeof unsafe === "function" && input !== undefined) {
    // Tools without arguments may omit `properties`, which some providers reject.
    return unsafe({ ...input, type: input.type ?? "object", ...(input.properties === undefined ? { properties: {} } : {}) });
  }
  return Type.Object({}, { additionalProperties: true });
}

type Content = { type: "text"; text: string } | { type: "image"; data: string; mimeType: string };

function modelContent(result: unknown): Content[] {
  if (result === null || result === undefined) return [];
  if (typeof result !== "object") return [{ type: "text", text: String(result) }];
  const r = result as { content?: Array<{ type?: string; text?: string; data?: string; mimeType?: string }>; structuredContent?: unknown };
  const out: Content[] = [];
  for (const part of Array.isArray(r.content) ? r.content : []) {
    if (part?.type === "text" && typeof part.text === "string") out.push({ type: "text", text: part.text });
    else if (part?.type === "image" && typeof part.data === "string" && typeof part.mimeType === "string")
      out.push({ type: "image", data: part.data, mimeType: part.mimeType });
  }
  if (out.length === 0) out.push({ type: "text", text: JSON.stringify(r.structuredContent ?? result) });
  return out;
}

function client(server: Server) {
  let nextId = 1;
  let session: string | undefined;
  const headers = (): Record<string, string> => ({
    ...server.headers,
    accept: "application/json, text/event-stream",
    "content-type": "application/json",
    "mcp-protocol-version": PROTOCOL,
    ...(session === undefined ? {} : { "mcp-session-id": session }),
  });
  const post = async (payload: unknown, signal?: AbortSignal) => {
    const response = await fetch(server.url, { method: "POST", headers: headers(), body: JSON.stringify(payload), signal });
    const id = response.headers.get("mcp-session-id");
    if (id) session = id;
    return response;
  };
  const request = async (method: string, params: unknown, signal?: AbortSignal) => {
    const response = await post({ jsonrpc: "2.0", id: nextId++, method, params }, signal);
    const body = await response.text();
    if (!response.ok) throw new Error(`MCP ${method} failed (${response.status}): ${body.slice(0, 400)}`);
    if (body.length === 0) return undefined;
    const reply = parseReply(body, response.headers.get("content-type") ?? "");
    if (reply.error) throw new Error(reply.error.message ?? `MCP ${method} returned an error`);
    return reply.result;
  };
  return {
    async connect(signal: AbortSignal) {
      await request("initialize", { protocolVersion: PROTOCOL, capabilities: {}, clientInfo: { name: "roll-pi", version: "1.0.0" } }, signal);
      await post({ jsonrpc: "2.0", method: "notifications/initialized", params: {} }, signal).catch(() => undefined);
    },
    async tools(signal: AbortSignal): Promise<McpTool[]> {
      const tools: McpTool[] = [];
      let cursor: string | undefined;
      for (let page = 0; page < MAX_TOOL_PAGES; page += 1) {
        const result = (await request("tools/list", cursor === undefined ? {} : { cursor }, signal)) as
          | { tools?: McpTool[]; nextCursor?: string }
          | undefined;
        tools.push(...(result?.tools ?? []));
        cursor = result?.nextCursor;
        if (!cursor) break;
      }
      return tools;
    },
    call: (name: string, args: Record<string, unknown>, signal?: AbortSignal) => request("tools/call", { name, arguments: args }, signal),
  };
}

export default async function rollExtension(pi: ExtensionAPI) {
  const readOnly = new Set<string>();

  pi.on("tool_call", async (event, ctx) => {
    const current = mode();
    if (current === "full_access") return;
    const builtinSearch =
      event.toolName === "tool_search" &&
      typeof pi.getAllTools === "function" &&
      pi.getAllTools().some((t) => t.name === "tool_search" && t.sourceInfo?.path === "builtin:tool-search");
    if (READ_ONLY_TOOLS.has(event.toolName) || builtinSearch || readOnly.has(event.toolName)) return;
    if (current === "auto_edit" && FILE_CHANGE_TOOLS.has(event.toolName)) return;
    const approved = await ctx.ui.confirm(`Allow ${event.toolName}?`, summary(event.input), { signal: ctx.signal });
    if (!approved) return { block: true, reason: `${event.toolName} was declined in roll.` };
  });

  const instructions = env("ROLL_PI_INSTRUCTIONS");
  if (instructions !== undefined) {
    pi.on("before_agent_start", (event) => {
      const sections = event.systemPromptOptions?.sections;
      if (sections && typeof sections === "object" && event.systemPromptOptions?.forceSystemPrompt === undefined) {
        sections.roll = instructions;
        return undefined;
      }
      return { systemPrompt: `${event.systemPrompt}\n\n${instructions}` };
    });
  }

  // Register at load: Pi keeps the first registration of a name, and extensions load first.
  const failures: string[] = [];
  for (const server of servers()) {
    const mcp = client(server);
    try {
      const signal = AbortSignal.timeout(CONNECT_TIMEOUT_MS);
      await mcp.connect(signal);
      for (const tool of await mcp.tools(signal)) {
        const name = `mcp__${server.name}__${tool.name}`;
        if (tool.annotations?.readOnlyHint === true) readOnly.add(name);
        pi.registerTool({
          name,
          label: tool.name,
          description: tool.description ?? tool.name,
          parameters: schema(tool.inputSchema),
          async execute(_toolCallId, params, signal) {
            const result = await mcp.call(tool.name, (params ?? {}) as Record<string, unknown>, signal);
            const content = modelContent(result);
            const isError = typeof result === "object" && result !== null && (result as { isError?: unknown }).isError === true;
            if (isError && !content.some((p) => p.type === "text" && p.text.length > 0))
              content.push({ type: "text", text: `MCP tool ${server.name}/${tool.name} returned an error` });
            return { content, structuredContent: result, details: { server: server.name, tool: tool.name }, ...(isError ? { isError: true } : {}) };
          },
        });
      }
    } catch (error) {
      failures.push(`${server.name}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  // roll shows notices only inside a turn, so say it at the first one.
  let told = failures.length === 0;
  pi.on("before_agent_start", (_event, ctx) => {
    if (told) return;
    told = true;
    ctx.ui.notify(`roll MCP unavailable (${failures.join("; ")})`, "warning");
  });
}
