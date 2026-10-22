import { Download } from "lucide-react";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Suspense, type ReactNode } from "react";
import { Breadcrumbs } from "@/components/repo/breadcrumbs";
import { CodeLines } from "@/components/repo/code-lines";
import { KeysTarget } from "@/components/repo/finder";
import { blobHref, joinPath, param } from "@/components/repo/paths";
import { RefPicker } from "@/components/repo/ref-picker";
import { LastChangeBar, Readme } from "@/components/repo/sections";
import { codeApi } from "@/lib/code-api";
import { TableSkeleton } from "@/components/repo/sections";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo, orNull, serverCode } from "@/lib/code-server";
import { highlight, HIGHLIGHT_MAX_BYTES, languageFor } from "@/lib/highlight";
import { requestNow } from "@/lib/now";
import { formatBytes } from "@/lib/roll-api";

type Props = PageProps<"/[owner]/[repo]/blob/[rev]/[...path]">;

const IMAGE = /\.(png|jpe?g|gif|webp|avif|svg|ico|bmp)$/i;
const MARKDOWN = /\.(md|markdown|mdx)$/i;
/** Text files larger than this get a download link instead of a view. */
const VIEW_MAX_BYTES = 2_000_000;

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { owner, repo, rev, path } = await params;
  return repoMetadata(owner, repo, `${joinPath(path)} at ${param(rev)}`);
}

/** One file: highlighted with line selection, rendered for markdown, shown for images. */
export default function BlobPage(props: Props) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <BlobPageContent {...props} />
    </Suspense>
  );
}

async function BlobPageContent({ params, searchParams }: Props) {
  const { owner, repo, rev: rawRev, path: parts } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  const r = found.repo;
  const rev = param(rawRev);
  const path = joinPath(parts);
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <RefPicker repo={r.full_name} rev={rev} kind="blob" path={path} />
        <Breadcrumbs repo={r.full_name} rev={rev} path={path} />
      </div>
      <Suspense
        key={`${rev}/${path}`}
        fallback={
          <div className="h-96 rounded-xl border border-line bg-panel p-4 text-[13px] text-subtle">
            <span className="shimmer">Reading {path.split("/").pop()}…</span>
          </div>
        }
      >
        <File repo={r.full_name} rev={rev} path={path} plain={searchParams.then((q) => q.plain === "1")} />
      </Suspense>
    </div>
  );
}

async function File({ repo, rev, path, plain: plainP }: { repo: string; rev: string; path: string; plain: Promise<boolean> }) {
  const [blob, log, now, plain] = await Promise.all([
    serverCode.blob(repo, rev, path),
    orNull(serverCode.log(repo, { rev, path, limit: 1 })),
    requestNow(),
    plainP,
  ]);
  if (!blob) notFound();
  const changeset = blob.changeset ?? log?.items[0]?.id ?? null;
  const permalink = changeset ? blobHref(repo, changeset, path) : null;
  const raw = codeApi.blobUrl(repo, changeset ?? rev, path);
  const size = blob.bytes.byteLength;
  const binary = blob.bytes.subarray(0, 8000).includes(0);

  const rawLink = (
    <a href={raw} className="inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-[12px] text-subtle transition hover:bg-panel-2 hover:text-fg">
      <Download className="size-3.5" /> Raw
    </a>
  );
  const frame = (body: ReactNode, info: ReactNode) => (
    <section className="overflow-hidden rounded-xl border border-line bg-panel">
      <KeysTarget rev={rev} permalink={permalink} />
      <LastChangeBar repo={repo} rev={rev} path={path} last={log?.items[0] ?? null} now={now} />
      {body ?? (
        <div className="flex items-center gap-1 border-b border-line px-3 py-1.5">
          <div className="mr-auto pl-1 font-mono text-[11px] text-subtle">{info}</div>
          {rawLink}
        </div>
      )}
    </section>
  );

  if (IMAGE.test(path)) {
    return (
      <>
        {frame(null, formatBytes(size))}
        <div className="grid place-items-center rounded-xl border border-line bg-panel p-8">
          {/* eslint-disable-next-line @next/next/no-img-element -- user content from the API */}
          <img src={raw} alt={path} className="max-h-[70vh] max-w-full" />
        </div>
      </>
    );
  }
  if (binary || size > VIEW_MAX_BYTES) {
    return (
      <>
        {frame(null, `${binary ? "binary" : "large file"} · ${formatBytes(size)}`)}
        <p className="rounded-xl border border-line bg-panel px-5 py-10 text-center text-[13px] text-subtle">
          {binary ? "Binary file" : "This file is too large to show"} —{" "}
          <a href={raw} className="text-accent hover:underline">
            download it
          </a>
          .
        </p>
      </>
    );
  }

  const text = new TextDecoder().decode(blob.bytes);
  const lines = text.replace(/\n$/, "").split("\n").length;
  const lang = languageFor(path);
  const info = (
    <>
      {lines.toLocaleString("en-US")} lines · {formatBytes(size)}
      {lang && <> · {lang}</>}
      {size > HIGHLIGHT_MAX_BYTES && " · not highlighted"}
    </>
  );

  if (MARKDOWN.test(path) && !plain) {
    return (
      <section className="overflow-hidden rounded-xl border border-line bg-panel">
        <KeysTarget rev={rev} permalink={permalink} />
        <LastChangeBar repo={repo} rev={rev} path={path} last={log?.items[0] ?? null} now={now} />
        <div className="flex items-center gap-1 border-b border-line px-3 py-1.5">
          <div className="mr-auto pl-1 font-mono text-[11px] text-subtle">{info}</div>
          <ViewToggle preview />
          {rawLink}
        </div>
        <Readme repo={repo} rev={changeset ?? rev} path={path} bare />
      </section>
    );
  }

  const tokens = await highlight(text, lang);
  return (
    <section className="overflow-hidden rounded-xl border border-line bg-panel">
      <KeysTarget rev={rev} permalink={permalink} />
      <LastChangeBar repo={repo} rev={rev} path={path} last={log?.items[0] ?? null} now={now} />
      <CodeLines
        tokens={tokens}
        text={text}
        permalink={permalink}
        info={info}
        blame={{ repo, rev: changeset ?? rev, path }}
        actions={
          <>
            {MARKDOWN.test(path) && <ViewToggle preview={false} />}
            {rawLink}
          </>
        }
      />
    </section>
  );
}

/** Preview / Code for markdown files, as links (`?plain=1`). */
function ViewToggle({ preview }: { preview: boolean }) {
  const cls = (on: boolean) => `rounded-md px-2 py-0.5 text-[12px] transition ${on ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`;
  return (
    <span className="mr-1 flex rounded-lg border border-line p-0.5">
      <Link href="?" className={cls(preview)} replace scroll={false}>
        Preview
      </Link>
      <Link href="?plain=1" className={cls(!preview)} replace scroll={false}>
        Code
      </Link>
    </span>
  );
}
