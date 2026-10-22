import type { Repo } from "@/lib/code-api";
import { CopyBlock } from "./clone-box";

/** What to do with an empty repo: push with git, or land through the API like an agent. */
export function QuickStart({ repo, apiUrl }: { repo: Repo; apiUrl: string }) {
  const git = `git remote add origin ${repo.clone_url}
git push -u origin ${repo.default_branch}`;
  const fresh = `echo "# ${repo.name}" > README.md
git init -b ${repo.default_branch} && git add . && git commit -m "First commit"
git remote add origin ${repo.clone_url}
git push -u origin ${repo.default_branch}`;
  const land = `curl -X POST ${apiUrl}/land \\
  -H "authorization: Bearer $ROLL_TOKEN" -H "content-type: application/json" \\
  -d '{"base": "${repo.default_branch}", "message": "First commit",
       "files": {"README.md": {"content": "# ${repo.name}\\n"}}}'`;
  return (
    <section className="overflow-hidden rounded-xl border border-line bg-panel">
      <div className="border-b border-line px-6 py-5">
        <h2 className="text-[17px] font-semibold tracking-[-0.03em]">This repository is empty</h2>
        <p className="mt-1 text-[13px] text-muted">Push an existing project, start a new one, or let an agent land the first change.</p>
      </div>
      <div className="space-y-6 px-6 py-5">
        <div className="space-y-2">
          <h3 className="text-[13px] font-medium">Push an existing repository</h3>
          <CopyBlock text={git} />
        </div>
        <div className="space-y-2">
          <h3 className="text-[13px] font-medium">Start from scratch</h3>
          <CopyBlock text={fresh} />
        </div>
        <div className="space-y-2">
          <h3 className="text-[13px] font-medium">Land without a checkout</h3>
          <CopyBlock text={land} />
          <p className="text-[12px] text-subtle">
            Agents never need a clone: read with <span className="font-mono">/tree</span> and{" "}
            <span className="font-mono">/blob</span>, write with <span className="font-mono">/land</span>. A conflict
            answers <span className="font-mono">409</span> with the paths.
          </p>
        </div>
      </div>
    </section>
  );
}
