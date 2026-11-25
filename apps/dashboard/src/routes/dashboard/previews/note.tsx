import { createFileRoute, useLocation } from "@tanstack/react-router";
import { NotePopup } from "@/components/workspace/preview-feedback";
import { pageHead } from "@/lib/head";

/** The preview page's feedback button opens this in a small window (`?preview=pv_…&fb_…`): the
 * note's form alone, posted as the person signed in to roll, then the window closes. */
export const Route = createFileRoute("/dashboard/previews/note")({
  head: () => pageHead({ title: "Note" }),
  component: function PreviewNoteRoute() {
    const { preview } = Route.useSearch() as { preview?: string };
    const query = useLocation({ select: (l) => l.searchStr });
    return <NotePopup preview={preview} query={query} />;
  },
});
