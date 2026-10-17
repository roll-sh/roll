import { HorizonMark, RollLogo, RollMark, type MarkState } from "@roll/brand";
import { Download } from "lucide-react";
import { Hero } from "@/components/hero";
import { SiteHeader } from "@/components/site-header";

const states: { state: MarkState; title: string; body: string }[] = [
  { state: "idle", title: "Idle", body: "Nodes breathe; one blue node marks your app, live." },
  { state: "thinking", title: "Thinking", body: "Meridians sweep the globe while the AI reasons." },
  { state: "deploying", title: "Deploying", body: "The globe settles into the horizon and rises, row by row." },
  { state: "success", title: "Success", body: "A ripple travels out from the core." },
  { state: "error", title: "Error", body: "Nodes jolt and cool to red." },
  { state: "sleep", title: "Sleep", body: "Scaled to zero: a dim horizon, barely breathing." },
];

const sizes = [16, 24, 32, 48, 64, 96];

const assets = [
  ["mark-light.svg", "Mark · on dark"],
  ["mark-dark.svg", "Mark · on light"],
  ["mark-accent-light.svg", "Mark + accent · on dark"],
  ["mark-accent-dark.svg", "Mark + accent · on light"],
  ["mark-fine-light.svg", "Fine mark · hero use"],
  ["horizon-light.svg", "Horizon · on dark"],
  ["horizon-dark.svg", "Horizon · on light"],
  ["avatar-dark.svg", "Profile picture · dark"],
  ["avatar-light.svg", "Profile picture · light"],
  ["avatar-blue.svg", "Profile picture · blue"],
] as const;

const avatars = ["avatar-dark.svg", "avatar-light.svg", "avatar-blue.svg"];

function Section({ id, title, lede, children }: { id: string; title: string; lede: string; children: React.ReactNode }) {
  return (
    <section id={id} className="mx-auto w-full max-w-6xl scroll-mt-20 px-4 py-20">
      <h2 className="text-2xl font-semibold tracking-[-0.04em]">{title}</h2>
      <p className="mt-2 max-w-lg text-[15px] text-muted">{lede}</p>
      <div className="mt-8">{children}</div>
    </section>
  );
}

export default function Home() {
  return (
    <>
      <SiteHeader />
      <main className="flex-1">
        <Hero />

        <Section id="states" title="One mark, six states" lede="The logo is the status indicator. The globe animates each state; the Horizon logo in the header dissolves into nodes and does the same.">
          <div className="grid grid-cols-1 gap-px overflow-hidden rounded-2xl border border-line bg-line sm:grid-cols-2 lg:grid-cols-3">
            {states.map((s) => (
              <div key={s.state} className="flex flex-col items-center bg-panel px-6 pt-10 pb-8 text-center">
                <RollMark state={s.state} size={132} className="text-fg" label={`roll mark, ${s.state}`} />
                <h3 className="mt-6 text-[15px] font-medium">{s.title}</h3>
                <p className="mt-1 max-w-[16rem] text-[13px] text-muted">{s.body}</p>
                <div className="mt-5 flex items-center gap-2 rounded-full border border-line px-3 py-1.5">
                  <HorizonMark state={s.state} size={26} interactive={false} className="text-fg" />
                  <span className="text-[13px] font-semibold tracking-[-0.05em]">roll</span>
                </div>
              </div>
            ))}
          </div>
        </Section>

        <Section id="profile" title="Profile picture" lede="Horizon is the social face of roll — a banded half-globe rising. Move your cursor toward it: the bands dissolve into the same nodes as the main mark.">
          <div className="grid gap-4 lg:grid-cols-5">
            <div className="flex h-96 flex-col items-center justify-center gap-6 rounded-2xl border border-line bg-panel lg:col-span-3">
              <div className="grid size-64 place-items-center rounded-full bg-panel-2">
                <HorizonMark size={190} className="text-fg" />
              </div>
              <p className="font-mono text-[11px] tracking-wide text-subtle uppercase">move closer</p>
            </div>
            <div className="flex flex-col justify-center gap-6 rounded-2xl border border-line bg-panel p-6 lg:col-span-2">
              {avatars.map((file) => (
                <div key={file} className="flex items-center gap-4">
                  {[72, 44, 28].map((px) => (
                    // eslint-disable-next-line @next/next/no-img-element
                    <img key={px} src={`/brand/${file}`} alt="" width={px} height={px} className="rounded-full" />
                  ))}
                  {/* eslint-disable-next-line @next/next/no-img-element */}
                  <img src={`/brand/${file}`} alt="" width={44} height={44} className="rounded-xl" />
                  <span className="ml-auto font-mono text-[11px] text-subtle">{file}</span>
                </div>
              ))}
            </div>
          </div>
        </Section>

        <Section id="sizes" title="Built for every size" lede="The Horizon logo holds from favicon to billboard. When it dissolves, small sizes use fewer, larger nodes.">
          <div className="flex flex-wrap items-end gap-8 rounded-2xl border border-line bg-panel p-8">
            {sizes.map((px) => (
              <div key={px} className="flex flex-col items-center gap-3">
                <HorizonMark size={px} className="text-fg" interactive={px >= 32} />
                <span className="font-mono text-[11px] text-subtle">{px}</span>
              </div>
            ))}
          </div>
          <div className="mt-4 grid gap-4 sm:grid-cols-2">
            <div className="flex h-36 items-center justify-center rounded-2xl border border-line bg-panel">
              <RollLogo size={36} />
            </div>
            <div className="flex h-36 items-center justify-center rounded-2xl bg-[#fafafa] text-[#0a0a0a]">
              <RollLogo size={36} />
            </div>
          </div>
        </Section>

        <Section id="assets" title="Assets" lede="Static SVGs generated from the same geometry as the animated mark (pnpm --filter @roll/brand export).">
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            {assets.map(([file, label]) => (
              <a
                key={file}
                href={`/brand/${file}`}
                download
                className="group flex items-center gap-4 rounded-xl border border-line bg-panel p-3 transition hover:border-line-2"
              >
                <span
                  className={`grid size-12 shrink-0 place-items-center rounded-lg ${file.includes("dark") ? "bg-[#fafafa]" : "bg-black"}`}
                >
                  {/* eslint-disable-next-line @next/next/no-img-element */}
                  <img src={`/brand/${file}`} alt="" className="size-9" />
                </span>
                <span className="min-w-0 flex-1 text-[13px]">
                  <span className="block truncate">{label}</span>
                  <span className="block truncate font-mono text-[11px] text-subtle">{file}</span>
                </span>
                <Download className="size-4 text-subtle group-hover:text-fg" />
              </a>
            ))}
          </div>
        </Section>
      </main>
      <footer className="border-t border-line py-8 text-center text-[12px] text-subtle">roll — serverless infrastructure for everything</footer>
    </>
  );
}
