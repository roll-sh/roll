import { useEffect, useRef, useState } from "react";
import { appsApi, deployChange, isPending, type DeployWatch, type Service } from "@/lib/apps-api";
import { service as serviceView } from "@/lib/workspace-resources";
import { workspace } from "@/lib/workspace-store";
import { useMarkState } from "../mark-state";

/** How often services are looked at: quickly while a deploy is on its way, calmly otherwise. */
export const POLL_BUSY_MS = 3000;
export const POLL_IDLE_MS = 15000;
/** While a page follows the service's changes: only a safety net. */
export const POLL_STREAMED_MS = 60000;

/**
 * Whether the page is visible. Polling waits while it's hidden (a background tab costs rolld
 * nothing) and looks again as soon as it's back.
 */
export function usePageVisible(): boolean {
  const [visible, setVisible] = useState(() => typeof document === "undefined" || document.visibilityState !== "hidden");
  useEffect(() => {
    const on = () => setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", on);
    return () => document.removeEventListener("visibilitychange", on);
  }, []);
  return visible;
}

export const anyPending = (list: Pick<Service, "environments">[]) => list.some((s) => s.environments.some((e) => e.latest && isPending(e.latest.status)));

/**
 * Drives the shared mark from the services' deploys: `deploying` while one is on its way, then
 * `success` or `error` when it ends. The first look only records what's there.
 */
export function useDeployMark(services: Pick<Service, "id" | "environments">[] | null) {
  const { play, setState } = useMarkState();
  const seen = useRef<DeployWatch["seen"] | null>(null);
  const busy = useRef(false);
  useEffect(() => {
    if (!services) return;
    const first = seen.current === null;
    const change = deployChange(seen.current ?? new Map(), services);
    seen.current = change.seen;
    if (change.pending) {
      if (!busy.current) setState("deploying");
      busy.current = true;
    } else if (!first && change.finished) {
      busy.current = false;
      play([[change.finished, change.finished === "error" ? 1600 : 1400]]);
    } else if (busy.current) {
      busy.current = false;
      setState("idle");
    }
  }, [services, play, setState]);
  // Leaving the page mid-deploy hands the mark back.
  useEffect(
    () => () => {
      if (busy.current) setState("idle");
    },
    [setState],
  );
}

/** A project's services, polled while the canvas shows them; their cards follow. */
export function useProjectServices(project: string, on: boolean) {
  const [services, setServices] = useState<Service[] | null>(null);
  const [poke, setPoke] = useState(0);
  const visible = usePageVisible();
  useEffect(() => {
    if (!on || !visible) return;
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      let next = POLL_IDLE_MS;
      try {
        const list = await appsApi.services(project);
        if (!alive) return;
        setServices(list);
        workspace.refreshViews(list.map(serviceView));
        if (anyPending(list)) next = POLL_BUSY_MS;
      } catch {
        // rolld away or apps off: try again later, the cards keep what they had.
      }
      if (alive) timer = setTimeout(loop, next);
    };
    void loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [project, on, poke, visible]);
  useDeployMark(services);
  /** `refresh` looks again now (after making a service), then keeps polling. */
  return { services, refresh: () => setPoke((n) => n + 1) };
}
