import { useCallback, useEffect, useRef, useState } from "react";
import { accessApi } from "@/lib/access-api";
import { getToken, operatorHeaders, subscribeToken } from "@/lib/code-token";

/**
 * Poll `fn` every `ms` while the page is visible, again when the signed-in token changes: what
 * every access view shows is live (an approval waits on it).
 */
export function usePoll<T>(fn: () => Promise<T>, ms: number) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const fnRef = useRef(fn);
  fnRef.current = fn;
  const refresh = useCallback(async () => {
    try {
      setData(await fnRef.current());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);
  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      if (document.visibilityState === "visible") await refresh();
      if (alive) timer = setTimeout(loop, ms);
    };
    void loop();
    const off = subscribeToken(() => void refresh());
    return () => {
      alive = false;
      clearTimeout(timer);
      off();
    };
  }, [ms, refresh]);
  return { data, error, refresh };
}

/** The approvals waiting right now (the tab's and the header's badge), polled while the operator's. */
export function usePending(ms = 5000) {
  const [can, setCan] = useState(mayManageAccess);
  useEffect(() => subscribeToken(() => setCan(mayManageAccess())), []);
  const { data } = usePoll(() => (can ? accessApi.approvals("pending") : Promise.resolve([])), ms);
  return can ? (data ?? []) : [];
}

export const usePendingCount = (ms = 5000) => usePending(ms).length;

/**
 * Whether this browser acts as the operator: signed in with the operator's token, or not signed
 * in at all (an open daemon). A roll code owner token never asks for approvals (it'd only get 401s).
 */
function mayManageAccess() {
  return !getToken() || Object.keys(operatorHeaders()).length > 0;
}

