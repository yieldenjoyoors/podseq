import { error } from "@sveltejs/kit";
import {
    docDescription,
    docExists,
    docSlugs,
    docTitle,
    renderDoc,
} from "#lib/docs";
import type { EntryGenerator, PageLoad } from "./$types";

export const entries: EntryGenerator = () =>
    docSlugs().map((slug) => ({ slug }));

export const load: PageLoad = ({ params }) => {
    // With `trailingSlash: "always"` a terminal rest param captures the trailing
    // slash ("setup/"), because the route pattern's optional final "/?" is
    // swallowed by the greedy rest match.
    const slug = params.slug.replace(/\/+$/, "");
    if (!docExists(slug)) error(404, "Doc not found");
    return {
        slug,
        title: docTitle(slug),
        description: docDescription(slug),
        rendered: renderDoc(slug),
    };
};
