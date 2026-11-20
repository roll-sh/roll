import { Mail } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { Link } from "@/components/link";
import { useSession } from "@/lib/accounts-api";
import { workspaceApi, type Invite } from "@/lib/workspace-api";
import { loadWorkspace } from "@/lib/workspace-store";
import { btn, EmptyPanel, ErrorNote, panel, primary } from "../ui";

const when = (ms: number) => new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });

/**
 * Invites to projects for the signed-in person (docs/members.md, "Invite and accept"). An invite's
 * link lands here with `?token=`: accepting it takes this session, signed in as the invited address.
 */
export function Invites({ token: linked }: { token?: string }) {
  const session = useSession();
  // The link's token, kept here and taken out of the address bar (and so out of history, a
  // screenshot or a shared URL) as soon as it's read.
  const [token] = useState(linked);
  useEffect(() => {
    if (linked && typeof window !== "undefined") window.history.replaceState(window.history.state, "", window.location.pathname);
  }, [linked]);
  const [list, setList] = useState<Invite[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [joined, setJoined] = useState<{ project: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const signedIn = session.status === "signed_in";

  const load = useCallback(() => {
    workspaceApi.myInvites().then(setList, (e: Error) => setError(e.message));
  }, []);
  useEffect(() => {
    if (signedIn) load();
  }, [signedIn, load]);

  const answer = async (accept: boolean, t: string) => {
    setBusy(true);
    setError(null);
    try {
      if (accept) {
        const m = await workspaceApi.accept(t);
        setJoined({ project: m.project });
        void loadWorkspace();
      } else {
        await workspaceApi.decline(t);
      }
      load();
    } catch (e) {
      setError((e as Error).message);
    }
    setBusy(false);
  };

  return (
    <div className="mx-auto flex w-full max-w-2xl flex-col gap-4 px-4 py-8">
      <div>
        <h1 className="text-[22px] font-semibold tracking-[-0.045em]">Invites</h1>
        <p className="text-[13px] text-muted">Projects people have invited you to. Each invite is for your address only.</p>
      </div>
      <ErrorNote error={error} />
      {!signedIn && session.status !== "loading" && (
        <EmptyPanel>
          <span className="text-[13px] text-muted">
            Sign in as the address the invite went to, then open its link again. <Link href={`/signin?return_to=${encodeURIComponent("/dashboard/invites")}`} className="text-accent">Sign in</Link>
          </span>
        </EmptyPanel>
      )}
      {joined && (
        <div className="rise rounded-xl border border-line bg-panel px-4 py-3 text-[13px]">
          You're in. <Link href={`/dashboard/projects/${encodeURIComponent(joined.project)}`} className="text-accent">Open the project</Link>
        </div>
      )}
      {signedIn && token && !joined && (
        <section className={`${panel} rise flex flex-col gap-3 p-4`}>
          <h2 className="text-[14px] font-medium">This link's invite</h2>
          <p className="text-[13px] text-muted">Accept it to join the project with the role it names. Nobody else can use it.</p>
          <div className="flex gap-2">
            <button className={primary} disabled={busy} onClick={() => answer(true, token)}>
              {busy ? <span className="shimmer">Joining…</span> : "Accept"}
            </button>
            <button className={btn} disabled={busy} onClick={() => answer(false, token)}>
              Decline
            </button>
          </div>
        </section>
      )}
      {signedIn &&
        (list === null ? (
          <div className="shimmer text-[13px] text-muted">Loading invites…</div>
        ) : list.length === 0 ? (
          !token && (
            <EmptyPanel>
              <Mail className="size-6 text-muted" strokeWidth={1.5} />
              <span className="text-[13px] text-muted">No invites waiting.</span>
            </EmptyPanel>
          )
        ) : (
          <section className={`${panel} overflow-hidden`}>
            <h2 className="border-b border-line px-4 py-2.5 text-[13px] font-medium">Waiting for you</h2>
            <ul className="divide-y divide-line">
              {list.map((i) => (
                <li key={i.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-3 text-[13px]">
                  <span className="min-w-0 flex-1 truncate font-medium">{i.project_name}</span>
                  <span className="text-muted">as {i.role}</span>
                  <span className="font-mono text-[12px] text-subtle">until {when(i.expires_at)}</span>
                  <span className="w-full text-[12px] text-subtle">Open the link in your invite email to accept or decline it.</span>
                </li>
              ))}
            </ul>
          </section>
        ))}
    </div>
  );
}
