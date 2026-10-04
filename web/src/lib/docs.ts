import { Marked, type RendererObject, type Tokens } from "marked";
import { markedHighlight } from "marked-highlight";
import hljs from "highlight.js/lib/common";

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

// Map GitHub fence names to highlight.js language ids the common languages
// bundle understands (e.g. `sh` -> `bash`).
function aliasFor(lang: string): string {
  const l = lang.toLowerCase();
  if (l === "sh" || l === "shell" || l === "console") return "bash";
  if (l === "txt") return "text";
  return l;
}

// Shared highlighter for any `<pre>` code block (docs + landing terminal).
export function highlightCode(code: string, lang: string): string {
  const language = aliasFor(lang || "text");
  if (language === "text") return code;
  if (hljs.getLanguage(language)) {
    try {
      return hljs.highlight(code, { language }).value;
    } catch {
      return code;
    }
  }
  try {
    return hljs.highlightAuto(code).value;
  } catch {
    return code;
  }
}

function escapeHtml(s: string): string {
  return s
    .replace(/&(?!#?\w+;)/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

// Plain text from rendered inline HTML (heading ids + outline text).
function plainText(html: string): string {
  return html
    .replace(/<[^>]*>/g, "")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'");
}

// Render a doc page to HTML without touching the DOM, so it works at build
// time (prerendering) and in the browser. Heading ids, internal links, and
// copy buttons are emitted directly by the renderers.
export function renderDoc(slug: string): RenderedDoc {
  const outline: OutlineItem[] = [];
  const seen = new Map<string, number>();

  const markedInstance = new Marked(markedHighlight({ highlight: highlightCode }));

  const renderer: RendererObject = {
    heading(token) {
      const inner = this.parser.parseInline(token.tokens);
      if (token.depth !== 2 && token.depth !== 3) {
        return `<h${token.depth}>${inner}</h${token.depth}>`;
      }

      const text = plainText(inner);
      let id = slugify(text);
      const count = seen.get(id) ?? 0;
      seen.set(id, count + 1);
      if (count > 0) id = `${id}-${count}`;
      outline.push({ id, text, level: token.depth });

      return (
        `<h${token.depth} id="${id}">${inner}` +
        `<a class="heading-anchor" href="#${id}" aria-label="Link to ${escapeHtml(text)}">#</a>` +
        `</h${token.depth}>`
      );
    },

    link(token) {
      const text = this.parser.parseInline(token.tokens);
      const title = token.title ? ` title="${escapeHtml(token.title)}"` : "";
      if (/^(https?:|mailto:)/.test(token.href)) {
        return `<a href="${token.href}"${title} target="_blank" rel="noopener">${text}</a>`;
      }
      if (/\.md($|#)/.test(token.href)) {
        const resolved = resolveDocLink(token.href, slug);
        if (resolved) {
          const href = resolved.anchor
            ? `/docs/${resolved.slug}/#${resolved.anchor}`
            : `/docs/${resolved.slug}/`;
          return `<a href="${href}" data-internal="true">${text}</a>`;
        }
      }
      return `<a href="${token.href}"${title}>${text}</a>`;
    },

    code(token) {
      const first = (token.lang || "").trim().split(/\s+/)[0] || "text";
      const lang = aliasFor(first);
      const body = token.escaped ? token.text : escapeHtml(token.text);
      return (
        `<pre data-lang="${lang}">` +
        `<code class="hljs language-${lang}">${body}</code>` +
        `<button class="doc-copy" type="button" aria-label="Copy code">copy</button>` +
        `</pre>`
      );
    },
  };

  markedInstance.use({ gfm: true, breaks: false, renderer });
  const html = markedInstance.parse(docContent(slug)) as string;
  return { html, outline };
}

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

// All doc slugs except the SUMMARY table of contents.
export function docSlugs(): string[] {
  return Object.keys(raw).filter((slug) => slug !== "SUMMARY");
}

// First prose line of a doc, for per-page meta descriptions.
export function docDescription(slug: string): string {
  for (const line of docContent(slug).split("\n")) {
    const t = line.trim();
    if (
      !t ||
      t.startsWith("#") ||
      t.startsWith("```") ||
      t.startsWith("---") ||
      t.startsWith("|") ||
      t.startsWith("<")
    ) {
      continue;
    }
    const text = t
      .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
      .replace(/[*_`]/g, "");
    if (!text) continue;
    return text.length > 160 ? `${text.slice(0, 157).trimEnd()}…` : text;
  }
  return "";
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
