/**
 * What can be done to one automation from anywhere it shows (a list row's hover actions, its
 * overview, the builder's header): pause or turn on, run now, duplicate, delete. Each answers with
 * a record the store takes at once; a failure is kept as `error` for the place to show.
 */

import { useState } from "react";
import type { Automation } from "@/lib/automations-api";
import { setAgentsView, type AutomationsView } from "@/lib/agents-surface";
import { specOf } from "@/lib/automations-ui";
import { useAutomations } from "./shared";

export function useAutomationActions(x: Automation | null, list?: AutomationsView["list"]) {
  const { store } = useAutomations();
  const [busy, setBusy] = useState<null | "toggle" | "run" | "duplicate" | "remove">(null);
  const [error, setError] = useState<string | null>(null);
  const act = async (what: NonNullable<typeof busy>, fn: () => Promise<void>) => {
    if (!store || !x) return;
    setError(null);
    setBusy(what);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  return {
    busy,
    error,
    setEnabled: (on: boolean) => act("toggle", async () => store!.put(on ? await store!.api.enable(store!.owner, x!.id) : await store!.api.disable(store!.owner, x!.id))),
    /** Runs it by hand and opens the run. */
    runNow: (input: unknown = null) =>
      act("run", async () => {
        const r = await store!.api.run(store!.owner, x!.id, { input });
        store!.putRun(r);
        setAgentsView({ kind: "automations", list, open: x!.id, tab: "runs", run: r.id });
      }),
    /** A paused copy, opened on its overview. */
    duplicate: () =>
      act("duplicate", async () => {
        const saved = await store!.api.create(store!.owner, { ...specOf(x!), name: `${x!.name} (copy)`, enabled: false });
        store!.put(saved.automation);
        setAgentsView({ kind: "automations", list, open: saved.automation.id });
      }),
    remove: (after?: () => void) =>
      act("remove", async () => {
        await store!.api.remove(store!.owner, x!.id);
        store!.forget(x!.id);
        after?.();
      }),
  };
}
