"use client";

/**
 * An admin's owner switcher (roll code dashboard: an admin sees every owner's repos; here, whose
 * agents and settings show). The shared picker over the server's owners: searchable, keyboard
 * throughout, recent picks first. A pick shows that owner everywhere at once (`chooseOwner`: the
 * agents, the header's crumb, the sidebar). Owners and open servers see their own owner only.
 */

import { useEffect, useState } from "react";
import { codeApi, type Owner } from "@/lib/code-api";
import { Picker, type PickerOption } from "./ui/picker";
import { chooseOwner } from "./use-connection";

export function OwnerSwitcher({ owner, className }: { owner: string; className?: string }) {
  const [owners, setOwners] = useState<readonly Owner[] | null>(null);
  useEffect(() => {
    let live = true;
    codeApi.owners().then(
      (list) => live && setOwners(list),
      () => live && setOwners([]),
    );
    return () => {
      live = false;
    };
  }, []);
  // Until the list is in, the shown owner alone (so the trigger names it).
  const options: PickerOption[] = owners
    ? owners.map((o) => ({ id: o.name, label: o.name, hint: o.kind === "org" ? "organisation" : undefined, icon: o.kind === "org" ? "group" : "user" }))
    : [{ id: owner, label: owner, icon: "user" }];
  return (
    <Picker
      label="Owner"
      searchPlaceholder="Find an owner…"
      options={options}
      recents="owners"
      value={owner}
      onChange={(v) => v && v !== owner && chooseOwner(v)}
      className={className}
    />
  );
}
