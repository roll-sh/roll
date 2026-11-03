"use client";

import { HorizonMark, RollMark } from "@roll/brand";
import { Check, KeyRound, Laptop } from "lucide-react";
import Link from "next/link";
import { useEffect, useState } from "react";
import { agentsApi, type MintedMachine, type PendingLogin } from "@/lib/agents-api";
import { codeApi, type Owner } from "@/lib/code-api";
import { btn, CopyButton, ErrorNote, input, Panel, primary, useCopy } from "../ui";

/** Owners the signed-in caller can connect machines to: itself, or every owner for the admin. */
function useOwners() {
  const [owners, setOwners] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    (async () => {
      try {
        const info = await codeApi.info();
        const c = info.auth?.caller;
        if (c?.kind === "owner" && c.owner) return live && setOwners([c.owner]);
        const all: Owner[] = await codeApi.owners();
        if (live) setOwners(all.map((o) => o.name));
      } catch (e) {
        if (live) setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      live = false;
    };
  }, []);
  return { owners, error };
}

/**
 * `/dashboard/agents/connect`: approve a device login (`roll agent login` opens this page with
 * `?code=`), or add a machine: the install steps, a code typed in by hand, or a token minted here.
 */
export function ConnectAgents({ code }: { code?: string }) {
  return (
    <div className="mx-auto max-w-2xl space-y-6">
      <div className="flex items-center gap-3">
        <HorizonMark size={28} />
        <h1 className="text-2xl font-semibold tracking-[-0.045em]">{code ? "Connect a machine" : "Add a machine"}</h1>
      </div>
      {code ? <Approve code={code} /> : <AddMachine />}
    </div>
  );
}

function Approve({ code }: { code: string }) {
  const { owners, error: ownersError } = useOwners();
  const [pending, setPending] = useState<PendingLogin | null>(null);
  const [owner, setOwner] = useState<string>("");
  const [state, setState] = useState<"loading" | "ready" | "approving" | "approved" | "denied" | "gone">("loading");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    agentsApi
      .pendingLogin(code)
      .then((p) => {
        if (!live) return;
        setPending(p);
        setState("ready");
      })
      .catch((e: Error & { status?: number }) => {
        if (!live) return;
        if (e.status === 404 || e.status === 410) setState("gone");
        else {
          setError(e.message);
          setState("ready");
        }
      });
    return () => {
      live = false;
    };
  }, [code]);
  useEffect(() => {
    if (owners?.length && !owner) setOwner(owners[0]);
  }, [owners, owner]);

  if (state === "gone")
    return (
      <Panel title="This code has expired">
        <p className="px-5 py-4 text-[13px] text-muted">
          Codes last ten minutes. Run <code className="font-mono text-fg">roll agent login</code> again on the machine.
        </p>
      </Panel>
    );
  if (state === "approved")
    return (
      <Panel title="Connected">
        <div className="flex items-center gap-4 px-5 py-5">
          <RollMark size={56} state="success" />
          <div className="space-y-2 text-[13px]">
            <p>
              <span className="font-medium">{pending?.hostname ?? "The machine"}</span> is signed in to <span className="font-mono">{owner}</span>. Start it with{" "}
              <code className="font-mono">roll agent up</code> and it appears on the canvas.
            </p>
            <Link href={`/dashboard/agents?owner=${encodeURIComponent(owner)}`} className="text-accent hover:underline">
              Open the canvas
            </Link>
          </div>
        </div>
      </Panel>
    );
  if (state === "denied")
    return (
      <Panel title="Denied">
        <p className="px-5 py-4 text-[13px] text-muted">The machine wasn&apos;t connected. Its login fails right away.</p>
      </Panel>
    );

  return (
    <Panel title="A machine wants to connect">
      <div className="space-y-4 px-5 py-4">
        <div className="flex items-center gap-3">
          <Laptop className="size-5 text-muted" />
          <div className="text-[13px]">
            <p className="font-medium">{pending?.hostname ?? (state === "loading" ? <span className="shimmer">Looking up the code…</span> : "Unknown machine")}</p>
            {pending?.os && <p className="text-subtle">{pending.os}</p>}
          </div>
          <span className="ml-auto rounded-lg border border-line px-3 py-1.5 font-mono text-[15px] tracking-[0.12em]">{code}</span>
        </div>
        <p className="text-[13px] text-muted">
          Check that this code matches the one <code className="font-mono">roll agent login</code> printed. The machine will run agents as the owner you pick, with that owner&apos;s projects and threads.
        </p>
        <label className="block space-y-1">
          <span className="text-[12px] text-subtle">Owner</span>
          <select value={owner} onChange={(e) => setOwner(e.target.value)} className={`${input} w-full`} disabled={!owners?.length}>
            {(owners ?? []).map((o) => (
              <option key={o}>{o}</option>
            ))}
          </select>
        </label>
        <div className="flex gap-2">
          <button
            className={primary}
            disabled={!owner || state !== "ready"}
            onClick={async () => {
              setState("approving");
              setError(null);
              try {
                await agentsApi.approveLogin(code, owner);
                setState("approved");
              } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
                setState("ready");
              }
            }}
          >
            {state === "approving" ? <span className="shimmer">Connecting…</span> : "Connect machine"}
          </button>
          <button
            className={btn}
            disabled={state !== "ready"}
            onClick={async () => {
              try {
                await agentsApi.denyLogin(code);
                setState("denied");
              } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
              }
            }}
          >
            Deny
          </button>
        </div>
        <ErrorNote error={error ?? ownersError} />
      </div>
    </Panel>
  );
}

function AddMachine() {
  const { owners, error: ownersError } = useOwners();
  const { copied, copy } = useCopy();
  const [owner, setOwner] = useState("");
  const [name, setName] = useState("");
  const [typed, setTyped] = useState("");
  const [minted, setMinted] = useState<MintedMachine | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (owners?.length && !owner) setOwner(owners[0]);
  }, [owners, owner]);

  const steps = ["curl -fsSL https://roll.sh/install.sh | sh", "roll agent login", "roll agent up"];
  return (
    <>
      <Panel title="On the machine">
        <ol className="space-y-3 px-5 py-4 text-[13px]">
          {[
            ["Install the roll CLI (it brings the agent daemon).", steps[0]],
            ["Sign in. It prints a code and opens this page to approve it.", steps[1]],
            ["Start the daemon. It connects out to roll: no ports, no tunnel.", steps[2]],
          ].map(([what, cmd], i) => (
            <li key={cmd} className="space-y-1.5">
              <p className="text-muted">
                <span className="mr-2 font-mono text-subtle">{i + 1}</span>
                {what}
              </p>
              <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-3 py-2">
                <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{cmd}</code>
                <CopyButton id={cmd} text={cmd} copied={copied} onCopy={copy} />
              </div>
            </li>
          ))}
        </ol>
        <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
          Agents use the logins already on the machine (<code className="font-mono">claude</code>, <code className="font-mono">codex</code>). roll never sees those credentials.
        </p>
      </Panel>

      <Panel title="Have a code?">
        <form
          className="flex gap-2 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            const c = typed.trim().toUpperCase();
            if (c) window.location.assign(`/dashboard/agents/connect?code=${encodeURIComponent(c)}`);
          }}
        >
          <input value={typed} onChange={(e) => setTyped(e.target.value)} placeholder="ABCD-EFGH" aria-label="Code" className={`${input} flex-1 font-mono tracking-[0.1em] uppercase`} />
          <button type="submit" className={primary} disabled={!typed.trim()}>
            Continue
          </button>
        </form>
      </Panel>

      <Panel title="Or make a machine token here" aside={<KeyRound className="size-4 text-subtle" />}>
        {minted ? (
          <div className="space-y-3 px-5 py-4 text-[13px]">
            <p className="flex items-center gap-2">
              <Check className="size-4 text-ok" /> <span className="font-medium">{minted.machine.name}</span> added. The token is shown once:
            </p>
            <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-3 py-2">
              <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{minted.token}</code>
              <CopyButton id="token" text={minted.token} copied={copied} onCopy={copy} />
            </div>
            <p className="text-muted">On the machine:</p>
            <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-3 py-2">
              <code className="min-w-0 flex-1 truncate font-mono text-[12px]">roll agent login --token {minted.token.slice(0, 8)}… && roll agent up</code>
              <CopyButton id="cmd" text={`roll agent login --token ${minted.token} && roll agent up`} copied={copied} onCopy={copy} />
            </div>
          </div>
        ) : (
          <form
            className="flex flex-wrap gap-2 px-5 py-4"
            onSubmit={async (e) => {
              e.preventDefault();
              setBusy(true);
              setError(null);
              try {
                setMinted(await agentsApi.addMachine(owner, name.trim()));
              } catch (err) {
                setError(err instanceof Error ? err.message : String(err));
              } finally {
                setBusy(false);
              }
            }}
          >
            {owners && owners.length > 1 && (
              <select value={owner} onChange={(e) => setOwner(e.target.value)} aria-label="Owner" className={input}>
                {owners.map((o) => (
                  <option key={o}>{o}</option>
                ))}
              </select>
            )}
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Machine name, e.g. build-box" aria-label="Machine name" className={`${input} min-w-0 flex-1`} />
            <button type="submit" className={primary} disabled={busy || !owner || !name.trim()}>
              {busy ? <span className="shimmer">Adding…</span> : "Add machine"}
            </button>
          </form>
        )}
        <div className="px-5 pb-4">
          <ErrorNote error={error ?? ownersError} />
        </div>
      </Panel>
    </>
  );
}
