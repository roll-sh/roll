"use client";

import { HorizonMark } from "@roll/brand";
import { ArrowLeft } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { codeApi, type CodeInfo, type Repo } from "@/lib/code-api";
import { CodeShell } from "./shell";
import { btn, Connecting, EmptyPanel, Offline } from "../ui";
import { POLL_MS } from "./shared";

/** Frame shared by the repo and change pages: polling the repo, crumbs, offline and not-found states. */

/** Poll the repo (`owner/name`); `missing` once the control plane answers but the repo isn't there. */
export function useRepo(fullName: string) {
  const [repo, setRepo] = useState<Repo | null>(null);
  const [info, setInfo] = useState<CodeInfo | null>(null);
  const [offline, setOffline] = useState(false);
  const [missing, setMissing] = useState(false);

  const refresh = useCallback(async () => {
    const [i, r] = await Promise.allSettled([codeApi.info(), codeApi.repo(fullName)]);
    if (i.status === "rejected") {
      setOffline(true);
      return;
    }
    setOffline(false);
    setInfo(i.value);
    if (r.status === "fulfilled") {
      setRepo(r.value);
      setMissing(false);
    } else setMissing(true);
  }, [fullName]);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await refresh();
      if (alive) timer = setTimeout(loop, POLL_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refresh]);

  return { repo, info, offline, missing, refresh };
}

/** Placeholder while the route's params resolve. */
export function CodeLoading() {
  return (
    <CodeShell crumb="code">
      <Connecting />
    </CodeShell>
  );
}

/** Not found / offline / connecting frames shared by the repo and change pages. */
export function RepoGate({
  id,
  crumb,
  offline,
  missing,
  ready,
  children,
}: {
  id: string;
  crumb: ReactNode;
  offline: boolean;
  missing: boolean;
  ready: boolean;
  children: () => ReactNode;
}) {
  if (offline) {
    return (
      <CodeShell crumb={crumb}>
        <Offline />
      </CodeShell>
    );
  }
  if (missing) {
    return (
      <CodeShell crumb={crumb}>
        <EmptyPanel>
          <HorizonMark state="sleep" size={40} />
          <h2 className="text-[15px] font-medium">No repository {id}</h2>
          <p className="max-w-md text-[13px] text-muted">It may have been deleted, or the link is wrong.</p>
          <Link href="/dashboard/code" className={btn}>
            <ArrowLeft className="size-3.5" /> All repositories
          </Link>
        </EmptyPanel>
      </CodeShell>
    );
  }
  if (!ready) {
    return (
      <CodeShell crumb={crumb}>
        <Connecting />
      </CodeShell>
    );
  }
  return <>{children()}</>;
}

/** `code / owner / repo [/ rest]`, each part linked. */
export function repoCrumb(fullName: string, rest?: ReactNode) {
  const [owner, name] = fullName.split("/");
  return (
    <>
      <Link href="/dashboard/code" className="text-muted hover:text-fg">
        code
      </Link>{" "}
      <span className="text-subtle">/</span>{" "}
      <Link href={`/dashboard/code/${owner}`} className="text-muted hover:text-fg">
        {owner}
      </Link>{" "}
      <span className="text-subtle">/</span>{" "}
      {rest ? (
        <>
          <Link href={`/dashboard/code/${fullName}`} className="text-muted hover:text-fg">
            {name}
          </Link>{" "}
          <span className="text-subtle">/</span> {rest}
        </>
      ) : (
        name
      )}
    </>
  );
}
