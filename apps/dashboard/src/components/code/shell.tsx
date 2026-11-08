import type { ReactNode } from "react";
import { AppShell } from "../app-shell";

/** The dashboard shell for roll code pages (sign-in is in every page's header). */
export function CodeShell({ crumb, actions, bleed, children }: { crumb: ReactNode; actions?: ReactNode; bleed?: boolean; children: ReactNode }) {
  return (
    <AppShell crumb={crumb} bleed={bleed} actions={actions}>
      {children}
    </AppShell>
  );
}
