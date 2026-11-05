"use client";

import { useEffect, useState } from "react";
import { codeApi } from "@/lib/code-api";
import { subscribeToken } from "@/lib/code-token";
import { clientFor, DEMO_OWNER, type AgentsClient } from "@/lib/agents-client";
import { httpServices, type AgentsServices } from "@/lib/agents-services";

const LAST_OWNER = "roll:agents:owner";

/**
 * The demo's in-memory owner, shared by its client and its services (loaded only when asked for).
 * `?demo=empty` starts it with no machines, to see the first-run screen.
 */
let demo: Promise<{ client: AgentsClient; services: AgentsServices }> | null = null;

function loadDemo() {
  demo ??= import("@/lib/agents-demo").then(({ DemoOwner }) => {
    const owner = new DemoOwner(1, true, new URLSearchParams(window.location.search).get("demo") === "empty");
    return { client: clientFor(DEMO_OWNER, () => owner), services: owner.services() };
  });
  return demo;
}

/** Which owner's agents to show: the signed-in owner, else the last one used here, else the first the server lists (admin and open servers). */
async function resolveOwner(): Promise<string | null> {
  const { auth } = await codeApi.info();
  if (auth?.caller.kind === "owner") return auth.caller.owner;
  // Signed out on a server that wants tokens: asking for anything would pop the sign-in on every page.
  if (auth?.required && auth.caller.kind !== "admin") return null;
  return localStorage.getItem(LAST_OWNER) ?? (await codeApi.owners())[0]?.name ?? null;
}

export interface AgentsConnection {
  client: AgentsClient | null;
  services: AgentsServices | null;
  /** Why there's no client (signed out, no owner yet, the server is down). */
  error: string | null;
}

let http: AgentsServices | null = null;

/** The client for the agents surface and the header: the demo, `owner`, or whoever is signed in (again after a sign-in). */
export function useAgentsConnection({ owner: param, demo: isDemo = false }: { owner?: string; demo?: boolean }): AgentsConnection {
  const [conn, setConn] = useState<AgentsConnection>({ client: null, services: null, error: null });
  const [token, setTokenVersion] = useState(0);
  useEffect(() => subscribeToken(() => setTokenVersion((n) => n + 1)), []);
  useEffect(() => {
    let live = true;
    (async () => {
      try {
        if (isDemo) {
          const d = await loadDemo();
          if (live) setConn({ ...d, error: null });
          return;
        }
        const o = param ?? (await resolveOwner());
        if (!live) return;
        if (!o) return setConn({ client: null, services: null, error: "Sign in to roll code to see your agents." });
        localStorage.setItem(LAST_OWNER, o);
        setConn({ client: clientFor(o), services: (http ??= httpServices()), error: null });
      } catch (e) {
        if (live) setConn({ client: null, services: null, error: e instanceof Error ? e.message : String(e) });
      }
    })();
    return () => {
      live = false;
    };
  }, [param, isDemo, token]);
  return conn;
}

/** `?demo` on the page's URL (read once, in the browser). */
export function useDemoParam(): boolean {
  const [d, setD] = useState(false);
  useEffect(() => setD(new URLSearchParams(window.location.search).has("demo")), []);
  return d;
}
