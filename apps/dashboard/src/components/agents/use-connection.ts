"use client";

import { useEffect, useState } from "react";
import { codeApi } from "@/lib/code-api";
import { getToken, subscribeToken } from "@/lib/code-token";
import { clientFor, DEMO_OWNER, type AgentsClient } from "@/lib/agents-client";
import { httpServices, type AgentsServices } from "@/lib/agents-services";

const LAST_OWNER = "roll:agents:owner";

/**
 * The demo's in-memory owner, shared by its client and its services (loaded only when asked for).
 * `?demo=empty` starts it with no machines, to see the first-run screen; `?demo=busy` gives one
 * project 30 more threads, to see the canvas at scale.
 */
let demo: Promise<{ client: AgentsClient; services: AgentsServices }> | null = null;

function loadDemo() {
  demo ??= import("@/lib/agents-demo").then(({ DemoOwner }) => {
    const params = new URLSearchParams(window.location.search);
    const variant = params.get("demo");
    // `?demo=1000`: that many more threads (at most 5000), to see the list and the board at scale.
    const many = Math.min(5000, Math.max(0, Number(variant) || 0));
    // `&still`: no new runs once the first ones end (frozen data, for the canvas checker's shots).
    const still = params.has("still");
    const owner = new DemoOwner(1, !still, variant === "empty", variant === "busy", many, still);
    return { client: clientFor(DEMO_OWNER, () => owner), services: owner.services() };
  });
  return demo;
}

/** Who the dashboard shows: an owner, or what's missing before there is one. */
type Resolved = { owner: string; admin: boolean } | { need: "sign-in" | "owner"; admin: boolean };

/** Which owner's agents to show: the signed-in owner, else the last one used here, else the first the server lists (admin and open servers). */
async function resolveOwner(): Promise<Resolved> {
  const { auth } = await codeApi.info();
  const admin = auth?.caller.kind === "admin";
  if (auth?.caller.kind === "owner" && auth.caller.owner) return { owner: auth.caller.owner, admin };
  // Signed out on a server that wants tokens: asking for anything would pop the sign-in on every page.
  if (auth?.required && !admin) return { need: "sign-in", admin };
  const owners = await codeApi.owners();
  const last = localStorage.getItem(LAST_OWNER);
  const owner = (last && owners.some((o) => o.name === last) ? last : null) ?? owners[0]?.name;
  return owner ? { owner, admin } : { need: "owner", admin };
}

// One resolution for every caller (the header's owner and the agents), until the token or the chosen owner changes.
let resolving: { key: string; p: Promise<Resolved> } | null = null;
function resolved(): Promise<Resolved> {
  const key = `${getToken() ?? ""}|${localStorage.getItem(LAST_OWNER) ?? ""}`;
  if (resolving?.key !== key) {
    const p = resolveOwner();
    resolving = { key, p };
    // A failure (the server was down) is asked again next time.
    p.catch(() => resolving?.p === p && (resolving = null));
  }
  return resolving.p;
}

/** The owner the dashboard shows now (`null`: none yet, or signed out). */
export function shownOwner(): Promise<string | null> {
  return resolved().then(
    (r) => ("owner" in r ? r.owner : null),
    () => null,
  );
}

const CHOSEN = "roll:agents:owner-chosen";

/** Show `owner` from now on (an admin's pick, a new owner): the agents and the header follow. */
export function chooseOwner(owner: string) {
  localStorage.setItem(LAST_OWNER, owner);
  resolving = null;
  window.dispatchEvent(new Event(CHOSEN));
}

function useResolveVersion() {
  const [version, setVersion] = useState(0);
  useEffect(() => {
    const bump = () => setVersion((n) => n + 1);
    const off = subscribeToken(bump);
    window.addEventListener(CHOSEN, bump);
    return () => {
      off();
      window.removeEventListener(CHOSEN, bump);
    };
  }, []);
  return version;
}

/** The owner the dashboard shows (`null` while signed out or before there's one; `undefined` while asking). */
export function useShownOwner(): { owner: string | null; admin: boolean } | undefined {
  const version = useResolveVersion();
  const [shown, setShown] = useState<{ owner: string | null; admin: boolean } | undefined>(undefined);
  useEffect(() => {
    let live = true;
    resolved().then(
      (r) => live && setShown({ owner: "owner" in r ? r.owner : null, admin: r.admin }),
      () => live && setShown(undefined),
    );
    return () => {
      live = false;
    };
  }, [version]);
  return shown;
}

export interface AgentsConnection {
  client: AgentsClient | null;
  services: AgentsServices | null;
  /** Why there's no client (signed out, no owner yet, the server is down). */
  error: string | null;
  /** What would give it one: signing in, or an owner to work as (none exists yet). */
  need?: "sign-in" | "owner";
  /** Signed in as admin: any owner can be shown (the owner switcher). */
  admin?: boolean;
}

let http: AgentsServices | null = null;

/** The client for the agents surface and the header: the demo, `owner`, or whoever is signed in (again after a sign-in). */
export function useAgentsConnection({ owner: param, demo: isDemo = false }: { owner?: string; demo?: boolean }): AgentsConnection {
  const [conn, setConn] = useState<AgentsConnection>({ client: null, services: null, error: null });
  const token = useResolveVersion();
  useEffect(() => {
    let live = true;
    (async () => {
      try {
        if (isDemo) {
          const d = await loadDemo();
          if (live) setConn({ ...d, error: null });
          return;
        }
        const r: Resolved = param ? { owner: param, admin: false } : await resolved();
        if (!live) return;
        if (!("owner" in r))
          return setConn({
            client: null,
            services: null,
            need: r.need,
            error: r.need === "sign-in" ? "Sign in to roll code to see your agents." : "There's no owner on this roll yet. Agents, machines and repos belong to one: you, or a team.",
          });
        const o = r.owner;
        localStorage.setItem(LAST_OWNER, o);
        setConn({ client: clientFor(o), services: (http ??= httpServices()), error: null, admin: r.admin });
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
