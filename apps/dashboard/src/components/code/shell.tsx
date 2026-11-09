import type { ReactNode } from "react";
import { AppShell } from "../app-shell";
import { SignIn } from "./sign-in";

/** The dashboard shell for roll code pages: the usual frame, plus sign-in when the server wants tokens. */
export function CodeShell({ crumb, actions, bleed, children }: { crumb: ReactNode; actions?: ReactNode; bleed?: boolean; children: ReactNode }) {
  return (
    <AppShell
      crumb={crumb}
      bleed={bleed}
      actions={
        <>
          {actions}
          <SignIn />
        </>
      }
    >
      {children}
    </AppShell>
  );
}
