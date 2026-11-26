import { createFileRoute, useLocation } from "@tanstack/react-router";
import { NotePopup, resolveFrom } from "@/components/workspace/preview-feedback";
import { ThreadPopup } from "@/components/workspace/preview-sheet";
import { pageHead } from "@/lib/head";

/** The preview page's feedback button opens this in a small window (`?preview=pv_…&fb_…`): the
 * note's form alone, posted as the person signed in to roll, then the window closes; its Reply and
 * Resolve (`fb_reply`/`fb_resolve`), the note's thread alone, closed once it's resolved. */
export const Route = createFileRoute("/dashboard/previews/note")({
  head: () => pageHead({ title: "Note" }),
  component: function PreviewNoteRoute() {
    const { preview } = Route.useSearch() as { preview?: string };
    const query = useLocation({ select: (l) => l.searchStr });
    // A Reply or Resolve names its note (`fb_reply`/`fb_resolve`); a new note carries `fb_note`.
    const review = resolveFrom(query);
    return review ? <ThreadPopup preview={preview} review={review} /> : <NotePopup preview={preview} query={query} />;
  },
});
