import { HorizonMark } from "@roll/brand";
import { LucideProvider, MonitorSmartphone } from "lucide-react";
import { useState } from "react";
import { accessApi } from "@/lib/access-api";
import { ago } from "@/lib/time";
import { SignIn } from "../code/sign-in";
import { input, primary } from "../ui";
import { AllowForm, LoadState, Problem, useSignedInLoad } from "./consent";

/**
 * `/oauth/device`: a device signing in to roll (RFC 8628, as `roll login --no-browser` does over
 * SSH). The person enters the code the device shows, sees what asks and from where, and chooses
 * what it may do, on the same form as the consent screen. Its code arrives in the link the device
 * printed (`?code=`), or is typed here.
 */
export function Device({ search }: { search: Record<string, unknown> }) {
  const [code, setCode] = useState(typeof search.code === "string" ? search.code : "");
  const [entered, setEntered] = useState(Boolean(code));
  return (
    <LucideProvider strokeWidth={1.5}>
      <div className="flex min-h-svh flex-col bg-bg">
        <header className="flex h-12 items-center justify-between border-b border-line px-4">
          <HorizonMark size={18} />
          <SignIn />
        </header>
        <main className="mx-auto w-full max-w-3xl flex-1 px-4 py-10">
          {entered ? (
            <Ask code={code} again={() => setEntered(false)} />
          ) : (
            <form
              className="rise rounded-xl border border-line bg-panel p-6"
              onSubmit={(e) => {
                e.preventDefault();
                if (code.trim()) setEntered(true);
              }}
            >
              <HorizonMark size={28} />
              <h1 className="mt-4 text-[15px] font-medium">Sign a device in to roll</h1>
              <p className="mt-1 text-[13px] text-muted">Enter the code your device shows.</p>
              <div className="mt-4 flex gap-2">
                <input
                  autoFocus
                  value={code}
                  onChange={(e) => setCode(e.target.value)}
                  placeholder="BCDF-GHJK"
                  maxLength={12}
                  autoCapitalize="characters"
                  autoComplete="off"
                  spellCheck={false}
                  className={`${input} w-44 font-mono uppercase tracking-[0.12em]`}
                />
                <button type="submit" className={primary} disabled={!code.trim()}>
                  Continue
                </button>
              </div>
            </form>
          )}
        </main>
      </div>
    </LucideProvider>
  );
}

function Ask({ code, again }: { code: string; again: () => void }) {
  const loaded = useSignedInLoad(() => accessApi.device(code), [code]);
  const [done, setDone] = useState<null | "allowed" | "waits" | "denied">(null);
  if (done) return <Done how={done} />;
  if (loaded.state === "error") {
    return (
      <div className="space-y-3">
        <Problem text={loaded.error} />
        <button onClick={again} className="text-[13px] text-accent">
          Enter another code
        </button>
      </div>
    );
  }
  if (loaded.state !== "ready") return <LoadState loaded={loaded} what="sign this device in" />;
  const { me, catalog, value: request } = loaded;
  const asked = ago(request.created_at / 1000, Date.now() / 1000);
  return (
    <AllowForm
      me={me}
      catalog={catalog}
      clientName={request.client.name}
      scope={request.scope}
      onAllow={async (a) => {
        const { pending } = await accessApi.allowDevice(request.user_code, a);
        setDone(pending ? "waits" : "allowed");
      }}
      onDeny={async () => {
        await accessApi.denyDevice(request.user_code);
        setDone("denied");
      }}
      head={(busy) => (
        <section className="flex items-start gap-4">
          <HorizonMark state={busy ? "thinking" : "idle"} size={36} interactive />
          <div className="min-w-0">
            <h1 className="text-xl font-semibold tracking-[-0.04em]">{request.client.name} wants to sign in</h1>
            <p className="mt-2 flex flex-wrap items-center gap-1.5 font-mono text-[12px] text-subtle">
              <MonitorSmartphone className="size-3.5" /> code {request.user_code} · asked {asked}
              {request.network ? ` from ${request.network}` : ""}
            </p>
            <p className="mt-2 text-[13px] text-muted">
              Only continue if you started this yourself, on a device in front of you, and its code matches. roll never sends you a code to
              enter: someone who asks you to is trying to get into your account.
            </p>
          </div>
        </section>
      )}
    />
  );
}

function Done({ how }: { how: "allowed" | "waits" | "denied" }) {
  const text = {
    allowed: ["The device is signed in", "Go back to it: it carries on by itself. Revoke it any time in MCP & access."],
    waits: ["The device is signed in, and waits for an admin", "It works once an admin approves it (they're mailed)."],
    denied: ["Refused", "The device isn't signed in. You can close this page."],
  }[how];
  return (
    <section className="rise rounded-xl border border-line bg-panel p-6">
      <HorizonMark state={how === "denied" ? "idle" : "success"} size={28} />
      <h1 className="mt-4 text-[15px] font-medium">{text[0]}</h1>
      <p className="mt-1 text-[13px] text-muted">{text[1]}</p>
    </section>
  );
}
