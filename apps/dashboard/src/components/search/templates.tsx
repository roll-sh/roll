import { SquareTerminal } from "lucide-react";
import { useEffect, useState } from "react";
import { esJson, type SearchCluster } from "@/lib/search-api";
import { btn, Panel } from "../ui";

/** A composable index template as `GET _index_template` lists it (the fields the panel reads). */
interface IndexTemplate {
  name: string;
  index_template: {
    index_patterns: string[];
    priority?: number;
    composed_of?: string[];
    data_stream?: unknown;
    template?: { lifecycle?: { data_retention?: string }; settings?: { index?: { lifecycle?: { name?: string } } }; mappings?: unknown; aliases?: Record<string, unknown> };
    _meta?: { managed?: boolean };
  };
}

/** What a template does to the indices it matches, in a few words. */
function effect(t: IndexTemplate["index_template"]): string {
  const parts: string[] = [];
  if (t.data_stream) parts.push("data stream");
  const retention = t.template?.lifecycle?.data_retention;
  if (retention) parts.push(`retention ${retention}`);
  const ilm = t.template?.settings?.index?.lifecycle?.name;
  if (ilm) parts.push(`ILM ${ilm}`);
  if (t.template?.mappings) parts.push("mappings");
  if (t.template?.settings && Object.keys(t.template.settings).length > (ilm ? 1 : 0)) parts.push("settings");
  const aliases = Object.keys(t.template?.aliases ?? {}).length;
  if (aliases) parts.push(`${aliases} alias${aliases === 1 ? "" : "es"}`);
  if (t.composed_of?.length) parts.push(`${t.composed_of.length} component${t.composed_of.length === 1 ? "" : "s"}`);
  return parts.join(" · ");
}

/**
 * A cluster's index templates: which index names each one matches, its priority and what it sets
 * (a data stream, the lifecycle, components). Shown once the cluster has a template of its own;
 * the ones Elasticsearch installs itself (`logs`, `metrics`, ...) are hidden unless asked for. A
 * template opens in the console to read or edit.
 */
export function Templates({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const [templates, setTemplates] = useState<IndexTemplate[] | null>(null);
  const [builtIn, setBuiltIn] = useState(false);

  useEffect(() => {
    let alive = true;
    esJson<{ index_templates: IndexTemplate[] }>(cluster.id, "GET", "/_index_template").then(
      (r) => alive && setTemplates(r.index_templates.sort((a, b) => (b.index_template.priority ?? 0) - (a.index_template.priority ?? 0) || a.name.localeCompare(b.name))),
      () => alive && setTemplates([]),
    );
    return () => {
      alive = false;
    };
  }, [cluster.id]);

  if (!templates) return null;
  const managed = templates.filter((t) => t.index_template._meta?.managed);
  const own = templates.filter((t) => !t.index_template._meta?.managed);
  // Every Elasticsearch cluster has built-in templates: the panel waits for one of the user's own.
  if (own.length === 0) return null;
  const shown = builtIn ? templates : own;

  return (
    <Panel
      title="Index templates"
      aside={
        managed.length > 0 && (
          <button onClick={() => setBuiltIn((b) => !b)} className={btn}>
            {builtIn ? "Hide built-in" : `Show ${managed.length} built-in`}
          </button>
        )
      }
    >
      <div className="overflow-x-auto">
          <table className="w-full text-[13px]">
            <thead>
              <tr className="border-b border-line text-left text-[11px] text-subtle">
                <th className="px-5 py-2 font-normal">Template</th>
                <th className="px-3 py-2 font-normal">Matches</th>
                <th className="px-3 py-2 text-right font-normal">Priority</th>
                <th className="px-3 py-2 font-normal">Sets</th>
                <th className="px-5 py-2" />
              </tr>
            </thead>
            <tbody>
              {shown.map((t) => (
                <tr key={t.name} className="border-b border-line last:border-0">
                  <td className="max-w-64 truncate px-5 py-2 font-mono text-[12px]">
                    {t.name}
                    {t.index_template._meta?.managed && <span className="ml-2 font-sans text-[11px] text-subtle">built-in</span>}
                  </td>
                  <td className="max-w-72 truncate px-3 py-2 font-mono text-[12px] text-muted">{t.index_template.index_patterns.join(", ")}</td>
                  <td className="whitespace-nowrap px-3 py-2 text-right font-mono text-[12px] tabular-nums">{t.index_template.priority ?? 0}</td>
                  <td className="px-3 py-2 text-[12px] text-muted">{effect(t.index_template)}</td>
                  <td className="px-5 py-2 text-right">
                    <button onClick={() => onOpenConsole(`GET /_index_template/${encodeURIComponent(t.name)}`)} className="rounded-md p-1 text-subtle transition hover:text-fg" title="Open in console">
                      <SquareTerminal className="size-3.5" />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
      </div>
    </Panel>
  );
}
