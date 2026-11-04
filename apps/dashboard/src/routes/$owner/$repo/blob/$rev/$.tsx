import { createFileRoute, notFound } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { Download } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "@/components/link";
import { Breadcrumbs } from "@/components/repo/breadcrumbs";
import { CodeLines } from "@/components/repo/code-lines";
import { KeysTarget } from "@/components/repo/finder";
import { blobHref } from "@/components/repo/paths";
import { RefPicker } from "@/components/repo/ref-picker";
import { LastChangeBar, Readme, TableSkeleton } from "@/components/repo/sections";
import { codeApi, type ChangesetSummary } from "@/lib/code-api";
import { repoHead } from "@/lib/code-meta";
import { nowSeconds, orNull, readDoc, serverCode, type Doc } from "@/lib/code-server";
import { highlight, HIGHLIGHT_MAX_BYTES, languageFor, type Token } from "@/lib/highlight";
import { or404 } from "@/lib/public-pages";
import { formatBytes } from "@/lib/roll-api";

const IMAGE = /\.(png|jpe?g|gif|webp|avif|svg|ico|bmp)$/i;
const MARKDOWN = /\.(md|markdown|mdx)$/i;
/** Text files larger than this get a download link instead of a view. */
const VIEW_MAX_BYTES = 2_000_000;

type TextInfo = { lines: number; lang: string | null; highlighted: boolean };
type FileView =
  | { kind: "image" }
  | { kind: "binary" }
  | { kind: "large" }
  | { kind: "markdown"; info: TextInfo; doc: Doc | null }
  | { kind: "code"; info: TextInfo; text: string; tokens: Token[][] | null };

interface FileData {
  changeset: string | null;
  last: ChangesetSummary | null;
  now: number;
  size: number;
  view: FileView;
}

/** One file, read and prepared on the server: the bytes never leave it, only what the page shows. */
const loadBlob = createServerFn({ method: "GET" })
  .validator((d: { repo: string; rev: string; path: string; plain: boolean }) => d)
  .handler(async ({ data: { repo, rev, path, plain } }): Promise<FileData | null> => {
    const code = serverCode();
    const [blob, log] = await Promise.all([code.blob(repo, rev, path), orNull(code.log(repo, { rev, path, limit: 1 }))]);
    if (!blob) return null;
    const changeset = blob.changeset ?? log?.items[0]?.id ?? null;
    const size = blob.bytes.byteLength;
    const binary = blob.bytes.subarray(0, 8000).includes(0);
    const base = { changeset, last: log?.items[0] ?? null, now: nowSeconds(), size };
    if (IMAGE.test(path)) return { ...base, view: { kind: "image" } };
    if (binary || size > VIEW_MAX_BYTES) return { ...base, view: { kind: binary ? "binary" : "large" } };

    const text = new TextDecoder().decode(blob.bytes);
    const lang = languageFor(path);
    const info = { lines: text.replace(/\n$/, "").split("\n").length, lang, highlighted: size <= HIGHLIGHT_MAX_BYTES };
    if (MARKDOWN.test(path) && !plain) return { ...base, view: { kind: "markdown", info, doc: await readDoc(repo, changeset ?? rev, path) } };
    return { ...base, view: { kind: "code", info, text, tokens: await highlight(text, lang) } };
  });

/** One file: highlighted with line selection, rendered for markdown, shown for images. */
export const Route = createFileRoute("/$owner/$repo/blob/$rev/$")({
  loaderDeps: ({ search }) => ({ plain: search.plain === "1" }),
  loader: async ({ context: { repo }, params: { rev, _splat: path = "" }, deps: { plain } }) => {
    // A file needs a path: `/blob/<rev>` alone is nothing.
    if (!path) throw notFound();
    return or404(await loadBlob({ data: { repo: repo.full_name, rev, path, plain } }));
  },
  head: ({ match, params: { rev, _splat: path } }) => repoHead(match.context.repo, `${path} at ${rev}`),
  component: BlobPage,
  pendingComponent: () => <TableSkeleton />,
});

function BlobPage() {
  const { repo: r } = Route.useRouteContext();
  const { rev, _splat: path = "" } = Route.useParams();
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <RefPicker repo={r.full_name} rev={rev} kind="blob" path={path} />
        <Breadcrumbs repo={r.full_name} rev={rev} path={path} />
      </div>
      <File repo={r.full_name} rev={rev} path={path} />
    </div>
  );
}

function File({ repo, rev, path }: { repo: string; rev: string; path: string }) {
  const { changeset, last, now, size, view } = Route.useLoaderData();
  const permalink = changeset ? blobHref(repo, changeset, path) : null;
  const raw = codeApi.blobUrl(repo, changeset ?? rev, path);

  const rawLink = (
    <a href={raw} className="inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-[12px] text-subtle transition hover:bg-panel-2 hover:text-fg">
      <Download className="size-3.5" /> Raw
    </a>
  );
  const frame = (info: ReactNode) => (
    <section className="overflow-hidden rounded-xl border border-line bg-panel">
      <KeysTarget rev={rev} permalink={permalink} />
      <LastChangeBar repo={repo} rev={rev} path={path} last={last} now={now} />
      <div className="flex items-center gap-1 border-b border-line px-3 py-1.5">
        <div className="mr-auto pl-1 font-mono text-[11px] text-subtle">{info}</div>
        {rawLink}
      </div>
    </section>
  );

  if (view.kind === "image") {
    return (
      <>
        {frame(formatBytes(size))}
        <div className="grid place-items-center rounded-xl border border-line bg-panel p-8">
          {/* User content from the API. */}
          <img src={raw} alt={path} className="max-h-[70vh] max-w-full" />
        </div>
      </>
    );
  }
  if (view.kind === "binary" || view.kind === "large") {
    const binary = view.kind === "binary";
    return (
      <>
        {frame(`${binary ? "binary" : "large file"} · ${formatBytes(size)}`)}
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

  const { lines, lang, highlighted } = view.info;
  const info = (
    <>
      {lines.toLocaleString("en-US")} lines · {formatBytes(size)}
      {lang && <> · {lang}</>}
      {!highlighted && " · not highlighted"}
    </>
  );

  if (view.kind === "markdown") {
    return (
      <section className="overflow-hidden rounded-xl border border-line bg-panel">
        <KeysTarget rev={rev} permalink={permalink} />
        <LastChangeBar repo={repo} rev={rev} path={path} last={last} now={now} />
        <div className="flex items-center gap-1 border-b border-line px-3 py-1.5">
          <div className="mr-auto pl-1 font-mono text-[11px] text-subtle">{info}</div>
          <ViewToggle preview />
          {rawLink}
        </div>
        {view.doc && <Readme doc={view.doc} bare />}
      </section>
    );
  }

  return (
    <section className="overflow-hidden rounded-xl border border-line bg-panel">
      <KeysTarget rev={rev} permalink={permalink} />
      <LastChangeBar repo={repo} rev={rev} path={path} last={last} now={now} />
      <CodeLines
        tokens={view.tokens}
        text={view.text}
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
      <Link href="?" className={cls(preview)} replace resetScroll={false}>
        Preview
      </Link>
      <Link href="?plain=1" className={cls(!preview)} replace resetScroll={false}>
        Code
      </Link>
    </span>
  );
}
