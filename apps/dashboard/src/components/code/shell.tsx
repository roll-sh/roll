"use client";

import type { ReactNode } from "react";
import { AppShell } from "../app-shell";
import { SignIn } from "./sign-in";

/** The dashboard shell for roll code pages: the usual frame, plus sign-in when the server wants tokens. */
export function CodeShell({ crumb, actions, children }: { crumb: ReactNode; actions?: ReactNode; children: ReactNode }) {
  return (
    <AppShell
      crumb={crumb}
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
