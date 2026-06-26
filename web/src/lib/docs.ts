import { marked } from "marked";

// `src/docs` is a symlink to the repo's `docs/` directory, so the markdown
// here is always the single source of truth.
const modules = import.meta.glob<string>("../docs/src/**/*.md", {
  query: "?raw",
  import: "default",
  eager: true,
});

export interface DocEntry {
  title: string;
  slug: string;
}

export interface DocSection {
  section: string;
  entries: DocEntry[];
}

export interface OutlineItem {
  id: string;
  text: string;
  level: number;
}

export interface RenderedDoc {
  html: string;
  outline: OutlineItem[];
}

export const DEFAULT_DOC = "README";

const raw: Record<string, string> = {};
for (const [path, content] of Object.entries(modules)) {
  const slug = path.replace(/^\.\.\/docs\/src\//, "").replace(/\.md$/, "");
  raw[slug] = content;
}

marked.setOptions({ gfm: true, breaks: false });

export function docContent(slug: string): string {
  return raw[slug] ?? raw[DEFAULT_DOC] ?? "";
}

export function docExists(slug: string): boolean {
  return slug in raw;
}

export function docTitle(slug: string): string {
  const match = docContent(slug).match(/^#\s+(.+)$/m);
  return match ? match[1].trim() : slug;
}

export function slugify(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^\w\s-]/g, "")
    .trim()
    .replace(/ /g, "-");
}

// Resolve a relative .md link from the current document into a route + anchor.
export function resolveDocLink(
  href: string,
  currentSlug: string,
): { slug: string; anchor: string | null } | null {
  if (!/\.md($|#)/.test(href)) return null;

  const [file, anchor] = href.split("#");
  const base = currentSlug.includes("/")
    ? currentSlug.replace(/\/[^/]*$/, "")
    : "";
  const parts = base ? base.split("/") : [];
  const segs = file.replace(/^\.\//, "").split("/");

  for (const seg of segs) {
    if (seg === "..") parts.pop();
    else if (seg === "." || seg === "") continue;
    else parts.push(seg);
  }

  const slug = parts.join("/").replace(/\.md$/, "");
  return { slug, anchor: anchor ? slugify(anchor) : null };
}

export function renderDoc(slug: string): RenderedDoc {
  const md = docContent(slug);
  const html = marked.parse(md) as string;

  if (typeof document === "undefined") return { html, outline: [] };

  const container = document.createElement("div");
  container.innerHTML = html;

  const outline: OutlineItem[] = [];
  const seen = new Map<string, number>();

  container.querySelectorAll("h2, h3").forEach((heading) => {
    const text = (heading.textContent || "").replace(/\s+/g, " ").trim();
    if (!text) return;
    let id = slugify(text);
    const count = seen.get(id) ?? 0;
    seen.set(id, count + 1);
    if (count > 0) id = `${id}-${count}`;
    heading.id = id;
    outline.push({ id, text, level: heading.tagName === "H2" ? 2 : 3 });
  });

  container.querySelectorAll("a").forEach((link) => {
    const href = link.getAttribute("href") || "";
    if (/^(https?:|mailto:)/.test(href)) {
      link.setAttribute("target", "_blank");
      link.setAttribute("rel", "noopener");
      return;
    }
    if (href.startsWith("/")) return;

    const resolved = resolveDocLink(href, slug);
    if (resolved) {
      link.setAttribute(
        "href",
        resolved.anchor
          ? `#/docs/${resolved.slug}~${resolved.anchor}`
          : `#/docs/${resolved.slug}`,
      );
      link.dataset.internal = "true";
    }
  });

  container.querySelectorAll("pre > code").forEach((code) => {
    const lang = [...code.classList]
      .find((c) => c.startsWith("language-"))
      ?.replace("language-", "");
    const pre = code.parentElement;
    if (pre) {
      pre.setAttribute("data-lang", lang || "text");
      const btn = document.createElement("button");
      btn.className = "doc-copy";
      btn.type = "button";
      btn.setAttribute("aria-label", "Copy code");
      btn.textContent = "copy";
      pre.appendChild(btn);
    }
  });

  return { html: container.innerHTML, outline };
}

export function parseSummary(): DocSection[] {
  const text = raw["SUMMARY"] ?? "";
  const sections: DocSection[] = [];
  let current: DocSection | null = null;

  for (const line of text.split("\n")) {
    const section = line.match(/^#\s+(.+)/);
    if (section) {
      if (section[1].trim().toLowerCase() === "summary") continue;
      current = { section: section[1].trim(), entries: [] };
      sections.push(current);
      continue;
    }
    const entry = line.match(/^-\s+\[([^\]]+)\]\(([^)]+)\)/);
    if (entry && current) {
      const slug = entry[2].replace(/^\.\//, "").replace(/\.md$/, "");
      current.entries.push({ title: entry[1], slug });
    }
  }

  return sections;
}

export const SECTIONS = parseSummary();
