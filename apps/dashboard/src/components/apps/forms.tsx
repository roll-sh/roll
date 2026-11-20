import { Eye, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, type FormSubmission, type SiteForm } from "@/lib/apps-api";
import { btn, Panel } from "../ui";
import { message, since, SureButton, type Runner } from "./shared";

/**
 * A static site's forms (Netlify Forms): what each has kept, and its submissions on request
 * (what people sent is a reveal, as variables' values are). Shown only once a form has one.
 */
export function FormsPanel({ service, env, runner }: { service: string; env: string; runner: Runner }) {
  const [forms, setForms] = useState<SiteForm[]>([]);
  const [open, setOpen] = useState<string | null>(null);
  const [subs, setSubs] = useState<FormSubmission[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    appsApi.forms(service, env).then(setForms, () => setForms([]));
  }, [service, env]);
  const show = (form: string) => {
    setOpen(form);
    setSubs(null);
    setError(null);
    appsApi.formSubmissions(service, env, form).then(setSubs, (e) => setError(message(e)));
  };
  if (forms.length === 0) return null;
  return (
    <Panel title="Forms" aside={<span className="text-[12px] text-muted">submissions to the site's marked forms, the newest 1,000 each</span>}>
      <ul className="divide-y divide-line">
        {forms.map((f) => (
          <li key={f.name} className="px-5 py-2.5 text-[12.5px]">
            <div className="flex flex-wrap items-center gap-3">
              <span className="font-medium">{f.name}</span>
              <span className="text-muted">
                {f.submissions} {f.submissions === 1 ? "submission" : "submissions"}, the last {since(f.last_at, Date.now())}
              </span>
              <button type="button" className={`${btn} ml-auto`} onClick={() => (open === f.name ? setOpen(null) : show(f.name))} aria-expanded={open === f.name}>
                <Eye className="size-3.5" /> {open === f.name ? "Hide" : "Show"}
              </button>
            </div>
            {open === f.name && (
              <div className="mt-2 space-y-2">
                {error && <p className="text-[12px] text-danger">{error}</p>}
                {subs?.length === 0 && <p className="text-[12px] text-subtle">None left.</p>}
                {subs?.map((s) => (
                  <div key={s.id} className="rounded-lg border border-line px-3 py-2">
                    <div className="mb-1 flex items-center justify-between text-[11px] text-subtle">
                      <span>{new Date(s.created_at).toLocaleString()}</span>
                      <SureButton
                        className="p-1 text-subtle hover:text-fg"
                        label="Delete this submission"
                        sure="Sure? Delete"
                        disabled={runner.busy !== null}
                        onSure={() => runner.run("form-delete", () => appsApi.deleteFormSubmission(service, env, f.name, s.id)).then(() => setSubs((l) => l?.filter((x) => x.id !== s.id) ?? null))}
                      >
                        <Trash2 className="size-3.5" />
                      </SureButton>
                    </div>
                    <dl className="grid grid-cols-[minmax(6rem,auto)_1fr] gap-x-3 gap-y-0.5">
                      {s.fields.map((x) => (
                        <div key={x.name} className="contents">
                          <dt className="font-mono text-[11.5px] text-muted">{x.name}</dt>
                          <dd className="whitespace-pre-wrap break-words text-[12px]">{x.value}</dd>
                        </div>
                      ))}
                    </dl>
                  </div>
                ))}
              </div>
            )}
          </li>
        ))}
      </ul>
    </Panel>
  );
}
