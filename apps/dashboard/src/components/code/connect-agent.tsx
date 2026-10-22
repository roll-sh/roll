"use client";

import { KeyRound, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { codeApi, type CodeInfo, type OwnerTokenInfo } from "@/lib/code-api";
import { mcpCommand, mcpJson, tokenLine } from "@/lib/ci";
import { ago, btn, CopyButton, input, Panel, useCopy } from "../ui";
import { Segmented } from "./shared";

/**
 * "Connect an agent": roll code as an MCP server. A copyable `claude mcp add` and the JSON most
 * clients take, plus (for the owner) a token made on the spot, shown once and dropped into both.
 */
export function ConnectAgent({
  owner,
  info,
  manage,
  onTokensChanged,
}: {
  owner: string;
  info: CodeInfo | null;
  manage: boolean;
  onTokensChanged?: () => void;
}) {
  const { copied, copy } = useCopy();
  const [format, setFormat] = useState<"claude" | "json">("claude");
  const [token, setToken] = useState<string | null>(null);
  const [name, setName] = useState("agent");
  const [making, setMaking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const url = codeApi.mcpUrl();
  // An open local daemon needs no token at all; otherwise the snippet carries one (or a placeholder).
  const open = info ? !info.auth.required : false;
  const shown = open && !token ? null : token;
  const snippet = format === "claude" ? mcpCommand(url, shown) : mcpJson(url, shown);

  const make = async () => {
    setMaking(true);
    setError(null);
    try {
      const t = await codeApi.createToken(owner, name.trim() || "agent");
      setToken(t.token);
      onTokensChanged?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setMaking(false);
    }
  };

  return (
    <Panel title="Connect an agent" aside={<Segmented value={format} options={[{ label: "Claude Code", value: "claude" }, { label: "JSON", value: "json" }]} onChange={setFormat} />}>
      <div className="space-y-3 px-5 py-4">
        <p className="text-[12px] text-subtle">
          roll code is an MCP server: agents search and read code, claim work, open changes, review and land, without a checkout.
        </p>
        <div className="relative rounded-lg border border-line bg-bg">
          <pre className="overflow-x-auto px-3 py-2 pr-9 font-mono text-[11px] leading-5">{snippet}</pre>
          <span className="absolute top-1.5 right-1.5">
            <CopyButton id="mcp" text={snippet} copied={copied} onCopy={copy} />
          </span>
        </div>
        {token ? (
          <p className="rise text-[11px] text-muted">
            The token is in the snippet above and is shown once: copy it now. It acts as <span className="font-mono">{owner}</span>; send{" "}
            <span className="font-mono">X-Roll-Agent</span> to name the agent.
          </p>
        ) : open ? (
          <p className="text-[11px] text-subtle">This daemon is open, so no token is needed. With a token configured, agents send an owner token.</p>
        ) : manage ? (
          <form
            className="flex flex-wrap items-center gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              make();
            }}
          >
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              aria-label="Token name"
              spellCheck={false}
              className={`${input} w-32 font-mono text-[12px]`}
            />
            <button type="submit" disabled={making} className={btn}>
              <KeyRound className="size-3.5" /> {making ? <span className="shimmer">Creating…</span> : `Create a token for ${owner}`}
            </button>
            {error && <span className="text-[11px] text-danger">{error}</span>}
          </form>
        ) : (
          <p className="text-[11px] text-subtle">
            Agents authenticate with an owner token: <span className="font-mono">roll code token create {owner} agent</span>.
          </p>
        )}
      </div>
    </Panel>
  );
}

/**
 * The owner's tokens: name, prefix and last use, never the token. Revoking one stops it at once,
 * over the API, git and MCP.
 */
export function AccessTokens({ owner, reload }: { owner: string; reload: number }) {
  const [tokens, setTokens] = useState<OwnerTokenInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);
  const load = useCallback(
    () => codeApi.tokens(owner).then(setTokens, (e) => setError(e instanceof Error ? e.message : String(e))),
    [owner],
  );
  useEffect(() => {
    load();
  }, [load, reload]);

  const revoke = async (t: OwnerTokenInfo) => {
    setRevoking(t.id);
    setError(null);
    try {
      await codeApi.revokeToken(owner, t.id);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setRevoking(null);
    }
  };

  return (
    <Panel title="Access tokens" aside={tokens && <span className="font-mono text-[12px] text-subtle">{tokens.length}</span>}>
      {error && <p className="px-5 pt-3 text-[12px] text-danger">{error}</p>}
      {tokens === null ? (
        <p className="px-5 py-4 text-[12px] text-subtle">
          <span className="shimmer">Loading…</span>
        </p>
      ) : tokens.length === 0 ? (
        <p className="px-5 py-4 text-[12px] text-subtle">
          No tokens yet. Agents, CI and git use one: <span className="font-mono">roll code token create {owner} agent</span>.
        </p>
      ) : (
        <ul className="divide-y divide-line">
          {tokens.map((t) => {
            const line = tokenLine(t, ago);
            return (
              <li key={t.id} className="flex items-center gap-3 px-5 py-2.5 text-[13px]">
                <KeyRound className="size-3.5 shrink-0 text-subtle" />
                <span className="min-w-0 truncate">{t.name}</span>
                <span className="font-mono text-[11px] text-subtle">{line.id}</span>
                <span className="ml-auto text-[11px] text-subtle">
                  created {ago(t.created_at)} · {line.used}
                </span>
                <button
                  type="button"
                  onClick={() => revoke(t)}
                  disabled={revoking !== null}
                  className="p-1 text-subtle transition hover:text-fg"
                  aria-label={`Revoke ${t.name}`}
                >
                  {revoking === t.id ? <span className="shimmer text-[11px]">Revoking…</span> : <Trash2 className="size-3.5" />}
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}
