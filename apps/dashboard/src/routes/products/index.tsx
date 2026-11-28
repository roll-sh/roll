import { createFileRoute } from "@tanstack/react-router";
import { Directory, Families } from "@/components/marketing/catalogue";
import { InstallCommand, MarketingFooter, MarketingHeader } from "@/components/marketing/chrome";
import { Reveal } from "@/components/marketing/fx";
import { HorizonScape } from "@/components/marketing/horizon-scape";
import { EYEBROW, H2, WRAP } from "@/components/marketing/layout";
import { useMarkState } from "@/components/mark-state";
import { pageHead } from "@/lib/head";
import { PRODUCTS } from "@/lib/marketing";

export const Route = createFileRoute("/products/")({
  head: () =>
    pageHead({
      title: "Products",
      description: "Every roll product: databases, streams, storage, search, hosting, source control, agents and secrets, in one daemon.",
    }),
  component: Products,
});

function Products() {
  const { state } = useMarkState();
  return (
    <>
      <MarketingHeader />
      <main className="flex-1">
        <section className="relative isolate overflow-hidden">
          <HorizonScape state={state} horizon={0.74} sun={false} className="-z-10" />
          <div className={`rise ${WRAP} flex flex-col items-center pt-20 pb-44 text-center sm:pt-28 sm:pb-52`}>
            <p className={EYEBROW}>{PRODUCTS.length} products · one daemon</p>
            <h1 className="mt-5 max-w-4xl text-5xl leading-[0.96] font-semibold tracking-[-0.06em] text-balance sm:text-[5.5rem]">One daemon. Every primitive.</h1>
            <p className="mt-6 max-w-2xl text-[16px] leading-relaxed text-muted sm:text-[18px]">
              Each product speaks the protocol you already use, branches like code, and costs nothing while it sleeps. Run them all from one{" "}
              <span className="font-mono text-[0.85em] text-fg">rolld</span>, or only the ones you need.
            </p>
            <InstallCommand className="mt-8 max-w-md bg-bg/70 backdrop-blur" />
          </div>
        </section>

        <section className={`${WRAP} relative -mt-14 pb-20 sm:-mt-20 sm:pb-24`}>
          <Reveal>
            <Directory />
          </Reveal>
        </section>

        <Families />

        <section className="border-t border-line">
          <Reveal className={`${WRAP} flex flex-col items-center py-24 text-center sm:py-32`}>
            <p className={EYEBROW}>Start</p>
            <h2 className={`mt-4 max-w-3xl ${H2}`}>One install. All of them.</h2>
            <p className="mt-5 max-w-xl text-[16px] leading-relaxed text-muted">
              The same <span className="font-mono text-[0.9em] text-fg">roll</span> creates a database, a stream, a bucket and a deploy. Nothing runs until something asks for it.
            </p>
            <InstallCommand className="mt-8 max-w-md" />
          </Reveal>
        </section>
      </main>
      <MarketingFooter />
    </>
  );
}
