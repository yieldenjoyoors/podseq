import { DEFAULT_DOC, docDescription, docTitle, renderDoc } from "#lib/docs";
import type { PageLoad } from "./$types";

// `/docs/` renders the introduction (README) — same content as `/docs/README/`.
export const load: PageLoad = () => {
    const slug = DEFAULT_DOC;
    return {
        slug,
        title: docTitle(slug),
        description: docDescription(slug),
        rendered: renderDoc(slug),
    };
};
