/**
 * Pickers over the owner's integrations (`lookup-api.ts`): `useLookupSource` turns a lookup
 * (`linear/teams`, `slack/channels`, a parent such as a team) into the shared picker's source, and
 * `LookupPicker` is the picker on it. The rows come from the server (the owner's token stays
 * there); a problem ("Reconnect Linear") links to the owner's settings.
 */

import { useMemo } from "react";
import { problemText, type LookupItem } from "@/lib/lookup-api";
import { Picker, type PickerOption, type PickerProps, type PickerSource } from "./ui/picker";
import { useAgents } from "./use-agents";

const SETTINGS = "#agents/settings/owner";

const option = (i: LookupItem): PickerOption => i;

/** A lookup as a picker source; `undefined` without services (nothing to ask). */
export function useLookupSource(source: string, kind: string, parent?: string | null): PickerSource | undefined {
  const { services, owner } = useAgents();
  const api = services?.lookup;
  return useMemo(() => {
    if (!api) return undefined;
    const p = parent ?? undefined;
    return {
      key: `${owner}/${source}/${kind}/${p ?? ""}`,
      load: async ({ q, cursor, ids }) => {
        const page = await api.list(owner, source, kind, { q, cursor, ids, parent: p });
        const pr = page.problem ? problemText(page.problem, source) : null;
        return {
          items: page.items.map(option),
          next: page.next,
          problem: pr ? { text: pr.text, action: pr.reconnect ? { label: page.problem!.code === "not_connected" ? "Connect" : "Reconnect", href: SETTINGS } : undefined } : null,
        };
      },
    };
  }, [api, owner, source, kind, parent]);
}

/** `Omit` over each half of the union, so `multiple` still picks `value` and `onChange`. */
type Without<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;

export type LookupPickerProps = Without<PickerProps, "source" | "onCreate"> & {
  from: { source: string; kind: string };
  /** The rows' parent (a team for its states). */
  parent?: string | null;
  /** New ones can be made from the search ("Create label …"). */
  create?: boolean;
};

export function LookupPicker({ from, parent, create, ...rest }: LookupPickerProps) {
  const { services, owner } = useAgents();
  const source = useLookupSource(from.source, from.kind, parent);
  const api = services?.lookup;
  const onCreate = create && api ? async (name: string) => option(await api.create(owner, from.source, from.kind, { name, parent: parent ?? undefined })) : undefined;
  // "Create label …": the kind, one of them.
  const noun = rest.noun ?? from.kind.replace(/s$/, "");
  return <Picker {...(rest as PickerProps)} source={source} onCreate={onCreate} noun={noun} recents={rest.recents ?? `${owner}/${from.source}/${from.kind}/${parent ?? ""}`} />;
}
