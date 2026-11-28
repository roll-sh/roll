import { createFileRoute } from "@tanstack/react-router";
import { ArrowRight } from "lucide-react";
import { useEffect, useRef } from "react";
import { Link } from "@/components/link";
import { Bento } from "@/components/marketing/bento";
import { AgentsChapter, CodeChapter, DataChapter, HostingChapter, MessagingChapter, Shipped } from "@/components/marketing/chapters";
import { InstallCommand, MarketingFooter, MarketingHeader } from "@/components/marketing/chrome";
import { DitherScape } from "@/components/marketing/dither-scape";
import { Reveal } from "@/components/marketing/fx";
import { HorizonScape } from "@/components/marketing/horizon-scape";
import { HowItWorks } from "@/components/marketing/how-it-works";
import { useSeen } from "@/components/marketing/in-view";
import { EYEBROW, H2, WRAP } from "@/components/marketing/layout";
import { Numbers } from "@/components/marketing/numbers";
import { Tour } from "@/components/marketing/shot";
import { useMarkState } from "@/components/mark-state";
import { pageHead } from "@/lib/head";
import { PRODUCTS, productHref } from "@/lib/marketing";

export const Route = createFileRoute("/")({
  head: () =>
    pageHead({
      description:
        "Postgres, Redis, Kafka, S3, Elasticsearch, Cassandra, RabbitMQ and your own apps: wire-compatible, branchable, and at zero when idle.",
      og: {
        title: "roll: serverless infrastructure for everything",
        description: "Wire-compatible databases, streams, storage, hosting, source control and agents, in one daemon.",
        type: "website",
      },
    }),
  component: Home,
});

function Home() {
  return (
    <>
      <MarketingHeader />
      <main className="flex-1">
        <Hero />
        <Products />
        <Overview />
        <DataChapter />
        <MessagingChapter />
        <HostingChapter />
        <CodeChapter />
        <AgentsChapter />
        <Measured />
        <Recent />
        <Closing />
      </main>
      <MarketingFooter />
    </>
  );
}

function Hero() {
  const { state } = useMarkState();
  return (
    <section className="relative isolate overflow-hidden">
      <div aria-hidden className="absolute inset-x-0 top-0 -z-10 h-[42rem] [mask-image:linear-gradient(to_bottom,black_70%,transparent)] sm:h-[48rem]">
        <HorizonScape state={state} horizon={0.6} />
      </div>
      <div className={`${WRAP} flex flex-col items-center pt-16 text-center sm:pt-28`}>
        <Link
          href={productHref("previews")}
          className="rise inline-flex items-center gap-2 rounded-full border border-line bg-bg/70 py-1 pr-3 pl-1 text-[12px] text-muted backdrop-blur transition hover:border-line-2 hover:text-fg"
        >
          <span className="rounded-full bg-accent/15 px-2 py-0.5 font-medium text-accent">New</span>
          Previews run your own Playwright tests
          <ArrowRight className="size-3" strokeWidth={1.5} />
        </Link>
        <h1 className="rise mt-7 max-w-6xl text-[2.9rem] leading-[0.96] font-semibold tracking-[-0.06em] text-balance [animation-delay:80ms] sm:text-[4.75rem] lg:text-[6rem]">
          Serverless infrastructure for everything.
        </h1>
        <p className="rise mt-7 max-w-3xl text-[16px] leading-relaxed text-muted text-balance [animation-delay:160ms] sm:text-[19px]">
          Postgres, Redis, Kafka, S3, Elasticsearch, Cassandra, RabbitMQ and your own apps. Compatible with the clients you have, branchable like
          code, and at zero when nobody's asking.
        </p>
        <div className="rise mt-9 flex w-full max-w-xl flex-col gap-3 [animation-delay:240ms] sm:flex-row">
          <Link
            href="/dashboard"
            className="inline-flex shrink-0 items-center justify-center gap-1.5 rounded-lg bg-fg px-5 py-2.5 text-[13.5px] font-medium text-bg transition hover:opacity-85 active:scale-[0.98]"
          >
            Start rolling <ArrowRight className="size-3.5" strokeWidth={1.5} />
          </Link>
          <InstallCommand className="bg-bg/70 backdrop-blur" />
        </div>
      </div>
      <Tour className={`rise ${WRAP} mt-14 pb-20 [animation-delay:360ms] sm:mt-20 sm:pb-28`} />
    </section>
  );
}

/** Every product on one screen: what it is, what it replaces, and a drawing of what it does. */
function Products() {
  return (
    <section id="products" className="scroll-mt-16 border-t border-line">
      <div className={`${WRAP} py-24 sm:py-32`}>
        <Reveal className="grid gap-6 lg:grid-cols-2 lg:gap-20">
          <div>
            <p className={EYEBROW}>{PRODUCTS.length} products · one daemon</p>
            <h2 className={`mt-4 ${H2}`}>Everything your app runs on.</h2>
          </div>
          <div className="lg:pt-9">
            <p className="max-w-lg text-[16.5px] leading-relaxed text-muted text-pretty">
              Each product speaks the protocol you already use, branches like code, and costs nothing while it sleeps. Start with one, or run them all.
            </p>
            <Link href="/products" className="mt-5 inline-flex items-center gap-1.5 text-[14px] text-fg transition hover:gap-2.5">
              All products <ArrowRight className="size-3.5" strokeWidth={1.5} />
            </Link>
          </div>
        </Reveal>
        <Reveal delay={100} className="mt-14">
          <Bento />
        </Reveal>
      </div>
    </section>
  );
}

/** How it holds together: a headline in the middle and the picture across the page, a request going through it. */
function Overview() {
  return (
    <section id="how" className="scroll-mt-16 border-t border-line">
      <div className="mx-auto w-full max-w-[112rem] px-5 py-24 sm:px-8 sm:py-36 lg:px-12">
        <Reveal className="text-center">
          <p className={EYEBROW}>How it works</p>
          <h2 className={`mx-auto mt-4 max-w-3xl ${H2}`}>One daemon, every product.</h2>
          <p className="mx-auto mt-5 max-w-2xl text-[16.5px] leading-relaxed text-muted text-pretty">
            roll answers each client in its own protocol, keeps the data in object storage, and runs nothing while nobody's asking.
          </p>
        </Reveal>
        <Reveal delay={120} className="mt-16 sm:mt-20">
          <HowItWorks />
        </Reveal>
      </div>
    </section>
  );
}

/** The measurements as rows, each drawn to scale beside what it's measured against. */
function Measured() {
  return (
    <section id="measured" className="scroll-mt-16 border-t border-line">
      <div className={`${WRAP} py-24 sm:py-32`}>
        <Reveal className="text-center">
          <p className={EYEBROW}>Measured</p>
          <h2 className={`mx-auto mt-4 max-w-3xl ${H2}`}>Fast at rest. Fast at work.</h2>
        </Reveal>
        <Reveal delay={100} className="mx-auto mt-16 max-w-6xl">
          <Numbers />
        </Reveal>
      </div>
    </section>
  );
}

function Recent() {
  return (
    <section id="shipped" className="scroll-mt-16 border-t border-line">
      <div className={`${WRAP} py-20 sm:py-24`}>
        <Reveal className="flex flex-wrap items-baseline justify-between gap-4">
          <h2 className="text-2xl font-semibold tracking-[-0.03em] text-fg sm:text-3xl">Recently shipped</h2>
          <Link href="/products" className="inline-flex items-center gap-1.5 text-[14px] text-fg transition hover:gap-2.5">
            All products <ArrowRight className="size-3.5" strokeWidth={1.5} />
          </Link>
        </Reveal>
        <Reveal delay={100} className="mt-8">
          <Shipped />
        </Reveal>
      </div>
    </section>
  );
}

/**
 * The last word, over the dithered landscape: Horizon comes up behind the ridge when the section
 * is first seen, and the shared mark plays a deploy, which lifts it and rings out from it.
 */
function Closing() {
  const ref = useRef<HTMLElement>(null);
  const seen = useSeen(ref);
  const { state, play } = useMarkState();
  useEffect(() => {
    if (seen) play([["deploying", 2400], ["success", 1600]]);
  }, [seen, play]);
  return (
    <section ref={ref} className="relative isolate overflow-hidden border-t border-line">
      <DitherScape state={state} className="-z-10" />
      <div className={`${WRAP} flex flex-col items-center pt-28 pb-[30rem] text-center sm:pt-32 sm:pb-[41rem]`}>
        <h2 className="max-w-3xl text-5xl leading-[0.98] font-semibold tracking-[-0.06em] text-balance sm:text-[5.5rem]">Start rolling.</h2>
        <p className="mt-6 max-w-md text-[16px] text-muted sm:text-[17px]">One command installs roll. Add --with-rolld to run the daemon yourself.</p>
        <div className="mt-9 flex w-full max-w-xl flex-col gap-3 sm:flex-row">
          <Link
            href="/dashboard"
            className="inline-flex shrink-0 items-center justify-center gap-1.5 rounded-lg bg-fg px-5 py-2.5 text-[13.5px] font-medium text-bg transition hover:opacity-85"
          >
            Open dashboard <ArrowRight className="size-3.5" strokeWidth={1.5} />
          </Link>
          <InstallCommand className="bg-bg/70 backdrop-blur" />
        </div>
      </div>
    </section>
  );
}
