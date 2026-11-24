import { Check, Globe, Play, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { type CrawlTest, esJson, type SearchCluster, searchApi, type SiteVerification } from "@/lib/search-api";
import { btn, input } from "../ui";
import { errText } from "./relevance-shared";

/** What a skipped URL's verdict means, in a few words. */
const VERDICTS: Record<string, string> = {
  allowed: "would be crawled",
  out_of_domain: "isn't on the crawl's domains",
  url_limit: "is over the URL limits",
  blocked: "is a blocked or private address",
  crawl_rule: "is excluded by a crawl rule",
  robots_disallowed: "is disallowed by robots.txt",
  robots_unreachable: "can't be crawled: robots.txt couldn't be read",
  origin_skipped: "is skipped for this crawl",
};

const say = (v: string) => VERDICTS[v.replace(/^redirected_/, "")] ?? v.replace(/_/g, " ");

/** A connector id and index name for a site: `docs.example.com` → `docs-example-com`. */
export function crawlerName(url: string): string {
  let host = url;
  try {
    host = new URL(url).hostname;
  } catch {
    // Not a URL yet: the text as typed.
  }
  return (
    host
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 60) || "site"
  );
}

/**
 * "Crawl a site": a URL is tested through the crawler's own path (`crawl-test`, nothing written),
 * then becomes a `roll_web_crawler` connector and its first crawl. Unverified sites get the small
 * limits; the site can be verified here too.
 */
export function CrawlSite({ cluster, onCreated }: { cluster: SearchCluster; onCreated: () => void }) {
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState<null | "test" | "create" | "verify" | "check">(null);
  const [result, setResult] = useState<CrawlTest | null>(null);
  const [verification, setVerification] = useState<SiteVerification | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = async <T,>(what: NonNullable<typeof busy>, f: () => Promise<T>): Promise<T | undefined> => {
    setBusy(what);
    setError(null);
    try {
      return await f();
    } catch (e) {
      setError(errText(e));
      return undefined;
    } finally {
      setBusy(null);
    }
  };
  const test = async () => {
    setResult(null);
    setVerification(null);
    const r = await run("test", () => searchApi.crawlTest(cluster.id, url.trim()));
    if (r) setResult(r);
  };
  const create = async () => {
    const name = crawlerName(url);
    const done = await run("create", async () => {
      await esJson(cluster.id, "PUT", `/_connector/${encodeURIComponent(name)}`, { index_name: `content-${name}`, is_native: true, service_type: "roll_web_crawler", name });
      await esJson(cluster.id, "PUT", `/_connector/${encodeURIComponent(name)}/_configuration`, { values: { urls: url.trim() } });
      await esJson(cluster.id, "POST", "/_connector/_sync_job", { id: name, job_type: "full", trigger_method: "on_demand" });
      return true;
    });
    if (done) onCreated();
  };
  const verify = async () => {
    const v = await run("verify", () => searchApi.verifySite(cluster.id, url.trim()));
    if (v) setVerification(v);
  };
  const check = async () => {
    if (!verification) return;
    const v = await run("check", () => searchApi.checkSite(cluster.id, verification.site));
    if (v) setVerification(v);
  };

  const skipped = result?.links?.filter((l) => l.verdict !== "allowed") ?? [];
  const queued = (result?.links?.length ?? 0) - skipped.length;
  const body = result?.document?.body ?? "";

  return (
    <div className="flex flex-col gap-3 px-5 py-5">
      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (url.trim()) void test();
        }}
      >
        <input
          aria-label="Site URL"
          className={`${input} min-w-0 flex-1 font-mono`}
          placeholder="https://docs.example.com/"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
        />
        <button type="submit" disabled={!url.trim() || busy !== null} className={btn}>
          <Globe className="size-3.5" /> {busy === "test" ? <span className="shimmer">Testing…</span> : "Test"}
        </button>
      </form>
      {error && <p className="text-[13px] text-danger">{error}</p>}
      {result && (
        <div className="rise flex flex-col gap-2 rounded-lg border border-line bg-panel px-4 py-3 text-[13px]">
          <p>
            <span className="font-mono text-[12px] text-subtle">{result.url}</span>{" "}
            <span className={result.verdict === "allowed" && result.indexed !== false ? "text-fg" : "text-danger"}>{say(result.verdict)}</span>
            {result.rule && <span className="text-subtle"> ({result.rule})</span>}
          </p>
          {result.robots && (
            <p className="text-[12px] text-muted">
              robots.txt {result.robots.read === "missing" ? "missing (everything allowed)" : result.robots.read}
              {result.robots.crawl_delay != null && <> · Crawl-delay {result.robots.crawl_delay}s</>}
            </p>
          )}
          {result.redirects?.map((r) => (
            <p key={r.from} className="font-mono text-[11px] text-subtle">
              {r.status} {r.from} → {r.to}
            </p>
          ))}
          {result.error && <p className="text-[12px] text-danger">{result.error}</p>}
          {result.indexed === false && result.reason && <p className="text-[12px] text-danger">Not indexed: {result.reason}</p>}
          {result.document?.title && <p className="font-medium">{result.document.title}</p>}
          {body && (
            <p className="line-clamp-3 text-[12px] text-muted">
              {body.slice(0, 400)}
              {body.length > 400 && "…"}
            </p>
          )}
          {result.links && (
            <p className="text-[12px] text-muted">
              {queued} link{queued === 1 ? "" : "s"} would be queued
              {skipped.length > 0 && <>, {skipped.length} skipped</>}
              {result.links_not_shown ? <> ({result.links_not_shown} more not shown)</> : null}
            </p>
          )}
          <div className="mt-1 flex flex-wrap items-center gap-2">
            <button onClick={create} disabled={busy !== null || result.verdict !== "allowed" || result.indexed === false} className={btn}>
              <Play className="size-3.5" /> {busy === "create" ? <span className="shimmer">Creating…</span> : `Crawl into content-${crawlerName(url)}`}
            </button>
            <button onClick={verify} disabled={busy !== null} className={btn}>
              <ShieldCheck className="size-3.5" /> Verify the site
            </button>
          </div>
          <p className="text-[12px] text-subtle">Until the site is verified, a crawl takes 1,000 URLs at most, a second apart, 4 times a day.</p>
        </div>
      )}
      {verification && (
        <div className="rise flex flex-col gap-2 rounded-lg border border-line bg-panel px-4 py-3 text-[13px]">
          <p>
            <span className="font-medium">{verification.site}</span>{" "}
            <span className={verification.status === "verified" ? "text-accent" : "text-muted"}>{verification.status}</span>
            {verification.proof && <span className="text-subtle"> · by {verification.proof === "dns" ? "DNS" : "file"}</span>}
          </p>
          {verification.status !== "verified" && (
            <>
              <p className="text-[12px] text-muted">Publish one of these, then check:</p>
              <p className="font-mono text-[12px]">
                TXT {verification.dns.name} “{verification.dns.value}”
              </p>
              <p className="font-mono text-[12px]">
                {verification.file.url} → {verification.file.content}
              </p>
              {verification.error && <p className="text-[12px] text-danger">{verification.error}</p>}
              <div>
                <button onClick={check} disabled={busy !== null} className={btn}>
                  <Check className="size-3.5" /> {busy === "check" ? <span className="shimmer">Checking…</span> : "Check"}
                </button>
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
