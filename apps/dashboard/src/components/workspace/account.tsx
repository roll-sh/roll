import { Check, ChevronsUpDown, LogOut, Settings, Users } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { codeApi, type Owner } from "@/lib/code-api";
import { requestSignIn, setToken } from "@/lib/code-token";
import { seedColor } from "@/lib/agents-identity";
import { showAgents } from "@/lib/agents-surface";
import { tint } from "../agents/identity";
import { chooseOwner, useShownOwner } from "../agents/use-connection";

const item = "flex w-full items-center gap-2.5 rounded-md px-2 py-1.5 text-left text-[12.5px] transition-colors hover:bg-white/[0.05]";

function Avatar({ name, size = 22 }: { name: string; size?: number }) {
  return (
    <span
      aria-hidden
      style={{ ...tint(seedColor(name)), width: size, height: size }}
      className="grid shrink-0 place-items-center rounded-full bg-(--tint)/20 text-[10px] font-semibold text-(--tint) uppercase"
    >
      {name[0]}
    </span>
  );
}

/**
 * Who's signed in, at the foot of the sidebar (Linear's account row): a menu that opens upward
 * with the owner to work as (admins can be any), the agents' settings and signing out.
 */
export function AccountMenu() {
  const shown = useShownOwner();
  const [open, setOpen] = useState(false);
  const [owners, setOwners] = useState<readonly Owner[] | null>(null);
  const [q, setQ] = useState("");
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const off = (e: PointerEvent) => !ref.current?.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    window.addEventListener("pointerdown", off);
    window.addEventListener("keydown", esc);
    return () => {
      window.removeEventListener("pointerdown", off);
      window.removeEventListener("keydown", esc);
    };
  }, [open]);

  useEffect(() => {
    if (!open || !shown?.admin || owners) return;
    codeApi.owners().then(setOwners, () => setOwners([]));
  }, [open, shown?.admin, owners]);

  if (shown === undefined) return <div className="h-9" />;
  if (!shown.owner)
    return (
      <button onClick={requestSignIn} className={`${item} mt-1 text-muted hover:text-fg`}>
        <LogOut className="size-3.5 rotate-180" />
        Sign in
      </button>
    );

  const owner = shown.owner;
  const list = (owners ?? []).filter((o) => o.name.includes(q.trim().toLowerCase()));
  return (
    <div ref={ref} className="relative mt-1">
      <button
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className={`flex w-full items-center gap-2.5 rounded-lg px-2 py-1.5 text-left transition-colors hover:bg-white/[0.04] ${open ? "bg-white/[0.05]" : ""}`}
      >
        <Avatar name={owner} />
        <span className="min-w-0 flex-1 leading-tight">
          <span className="block truncate text-[12.5px] font-medium">{owner}</span>
          <span className="block text-[10.5px] text-subtle">{shown.admin ? "Admin" : "Signed in"}</span>
        </span>
        <ChevronsUpDown className="size-3.5 text-subtle" />
      </button>

      <div
        className={`absolute bottom-[calc(100%+6px)] left-0 z-30 w-full origin-bottom-left rounded-xl border border-line-2 bg-popover p-1 shadow-pop inset-shadow-highlight transition-[opacity,transform] duration-150 ease-out-soft motion-reduce:transition-none ${
          open ? "scale-100 opacity-100" : "pointer-events-none scale-95 opacity-0"
        }`}
        inert={!open}
      >
        {shown.admin && (
          <>
            <div className="flex items-center gap-1.5 px-2 pt-1.5 pb-1 text-[10.5px] font-medium text-subtle">
              <Users className="size-3" /> Work as
            </div>
            {owners && owners.length > 6 && (
              <input
                value={q}
                onChange={(e) => setQ(e.target.value)}
                placeholder="Find an owner…"
                className="mx-1 mb-1 h-7 w-[calc(100%-0.5rem)] rounded-md border border-line-2 bg-canvas px-2 text-[12px] outline-none focus:border-accent/60"
              />
            )}
            <div className="max-h-56 overflow-y-auto [scrollbar-width:none]">
              {(owners ? list : [{ name: owner } as Owner]).map((o) => (
                <button
                  key={o.name}
                  onClick={() => {
                    if (o.name !== owner) chooseOwner(o.name);
                    setOpen(false);
                  }}
                  className={item}
                >
                  <Avatar name={o.name} size={18} />
                  <span className="min-w-0 flex-1 truncate">{o.name}</span>
                  {o.kind === "org" && <span className="text-[10.5px] text-subtle">team</span>}
                  {o.name === owner && <Check className="size-3.5 text-accent" />}
                </button>
              ))}
            </div>
            <div className="my-1 h-px bg-line" />
          </>
        )}
        <button
          onClick={() => {
            setOpen(false);
            showAgents({ kind: "settings", target: "owner" });
          }}
          className={`${item} text-muted hover:text-fg`}
        >
          <Settings className="size-3.5" /> Settings
        </button>
        <button
          onClick={() => {
            setOpen(false);
            setToken(null);
          }}
          className={`${item} text-muted hover:text-fg`}
        >
          <LogOut className="size-3.5" /> Sign out
        </button>
      </div>
    </div>
  );
}
