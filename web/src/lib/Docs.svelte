<script lang="ts">
    import { SECTIONS, DEFAULT_DOC, renderDoc } from "./docs";

    let { doc, anchor = null }: { doc: string; anchor?: string | null } =
        $props();

    let query = $state("");
    let sidebarOpen = $state(false);
    let contentEl = $state<HTMLElement | null>(null);

    const rendered = $derived(renderDoc(doc));

    const flat = $derived(SECTIONS.flatMap((s) => s.entries));
    const index = $derived(flat.findIndex((e) => e.slug === doc));
    const prev = $derived(index > 0 ? flat[index - 1] : null);
    const next = $derived(
        index >= 0 && index < flat.length - 1 ? flat[index + 1] : null,
    );

    const filtered = $derived(
        query.trim()
            ? SECTIONS.map((s) => ({
                  ...s,
                  entries: s.entries.filter((e) =>
                      e.title
                          .toLowerCase()
                          .includes(query.trim().toLowerCase()),
                  ),
              })).filter((s) => s.entries.length)
            : SECTIONS,
    );

    // Scroll to anchor (or top) whenever the document or anchor changes.
    $effect(() => {
        void rendered.html;
        const a = anchor;
        const el = contentEl;
        if (!el) return;

        const apply = () => {
            if (a) {
                const target = el.querySelector(`#${CSS.escape(a)}`);
                if (target) {
                    target.scrollIntoView({
                        behavior: "smooth",
                        block: "start",
                    });
                    return;
                }
            }
            window.scrollTo({ top: 0, behavior: "auto" });
        };

        const reduce = window.matchMedia(
            "(prefers-reduced-motion: reduce)",
        ).matches;
        if (reduce) {
            apply();
        } else {
            const raf = requestAnimationFrame(apply);
            return () => cancelAnimationFrame(raf);
        }
    });

    // Copy buttons inside rendered code blocks (event delegation).
    $effect(() => {
        const el = contentEl;
        if (!el) return;

        const onClick = async (e: MouseEvent) => {
            const btn = (e.target as HTMLElement).closest(
                ".doc-copy",
            ) as HTMLButtonElement | null;
            if (!btn) return;
            const pre = btn.parentElement;
            const code = pre?.querySelector("code");
            if (!code) return;
            try {
                await navigator.clipboard.writeText(code.textContent ?? "");
                btn.textContent = "copied";
                btn.classList.add("copied");
                window.setTimeout(() => {
                    btn.textContent = "copy";
                    btn.classList.remove("copied");
                }, 1300);
            } catch {
                // clipboard unavailable
            }
        };

        el.addEventListener("click", onClick);
        return () => el.removeEventListener("click", onClick);
    });
</script>

<div class="docs-layout">
    <!-- mobile toggle -->
    <button
        class="contents-toggle"
        onclick={() => (sidebarOpen = !sidebarOpen)}
    >
        <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
        >
            <path d="M3 6h18M3 12h18M3 18h18" />
        </svg>
        Contents
    </button>

    {#if sidebarOpen}
        <button
            class="docs-backdrop"
            aria-label="Close menu"
            onclick={() => (sidebarOpen = false)}
        ></button>
    {/if}

    <!-- sidebar -->
    <aside class="docs-side" class:open={sidebarOpen}>
        <div class="side-search">
            <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
            >
                <circle cx="11" cy="11" r="7" />
                <path d="M21 21l-4.3-4.3" />
            </svg>
            <input type="text" placeholder="Filter docs…" bind:value={query} />
        </div>

        <nav class="side-nav">
            {#each filtered as section (section.section)}
                <div class="side-section">
                    <p class="side-heading">{section.section}</p>
                    {#each section.entries as entry (entry.slug)}
                        <a
                            href={`#/docs/${entry.slug}`}
                            class="side-link"
                            class:active={entry.slug === doc}
                            onclick={() => (sidebarOpen = false)}
                        >
                            {entry.title}
                        </a>
                    {/each}
                </div>
            {/each}
        </nav>

        <div class="side-foot">
            <a
                href="#/"
                class="micro text-[var(--muted)] hover:text-[var(--brand)] transition-colors"
            >
                ← back to overview
            </a>
        </div>
    </aside>

    <!-- main -->
    <main class="docs-main">
        <div class="docs-breadcrumb">
            <a href="#/docs">docs</a>
            <span class="sep">/</span>
            <span
                >{doc === DEFAULT_DOC
                    ? "introduction"
                    : doc.replace(/\//g, " / ")}</span
            >
        </div>

        <article class="prose-docs" bind:this={contentEl}>
            {@html rendered.html}
        </article>

        <div class="docs-pager">
            {#if prev}
                <a class="pager-link" href={`#/docs/${prev.slug}`}>
                    <span class="docs-kicker">prev</span>
                    <span class="pager-title">{prev.title}</span>
                </a>
            {:else}
                <span></span>
            {/if}
            {#if next}
                <a class="pager-link right" href={`#/docs/${next.slug}`}>
                    <span class="docs-kicker">next</span>
                    <span class="pager-title"
                        >{next.title}
                        <svg
                            width="13"
                            height="13"
                            viewBox="0 0 24 24"
                            fill="none"
                            stroke="currentColor"
                            stroke-width="2.4"
                        >
                            <path d="M5 12h14M13 6l6 6-6 6" />
                        </svg>
                    </span>
                </a>
            {/if}
        </div>
    </main>

    <!-- outline -->
    <aside class="docs-outline">
        {#if rendered.outline.length}
            <p class="docs-kicker mb-3">On this page</p>
            <nav>
                {#each rendered.outline as item (item.id)}
                    <a
                        href={`#/docs/${doc}~${item.id}`}
                        class="outline-link"
                        class:sub={item.level === 3}
                    >
                        {item.text}
                    </a>
                {/each}
            </nav>
        {/if}
    </aside>
</div>

<style>
    .docs-layout {
        display: grid;
        grid-template-columns: 1fr;
        max-width: 100rem;
        margin: 0 auto;
        padding: 0 1.25rem;
        gap: 0;
        position: relative;
    }

    .contents-toggle {
        display: inline-flex;
        align-items: center;
        gap: 0.5rem;
        font-size: 0.7rem;
        font-weight: 700;
        text-transform: uppercase;
        letter-spacing: 0.04em;
        color: var(--fg);
        margin: 1.25rem 0 0.5rem;
        padding: 0.5rem 0.8rem;
        border: 2px solid var(--border);
        background: var(--bg);
        width: fit-content;
    }

    .docs-backdrop {
        position: fixed;
        inset: 0;
        top: 3.5rem;
        background: rgba(10, 10, 10, 0.4);
        z-index: 40;
        border: none;
        cursor: pointer;
    }

    .docs-side {
        display: none;
    }

    .docs-side.open {
        display: block;
        position: fixed;
        top: 3.5rem;
        left: 0;
        bottom: 0;
        width: 17rem;
        background: var(--bg);
        border-right: 2px solid var(--border);
        z-index: 45;
        padding: 1rem 1rem 2rem;
        overflow-y: auto;
    }

    .side-search {
        display: flex;
        align-items: center;
        gap: 0.5rem;
        padding: 0.5rem 0.7rem;
        border: 2px solid var(--border);
        background: var(--bg);
        color: var(--muted);
        margin-bottom: 1.2rem;
    }
    .side-search input {
        background: none;
        border: none;
        outline: none;
        color: var(--fg);
        font-size: 0.85rem;
        width: 100%;
    }
    .side-search input::placeholder {
        color: var(--faint);
    }

    .side-section {
        margin-bottom: 1.4rem;
    }
    .side-heading {
        font-size: 0.7rem;
        letter-spacing: 0.18em;
        text-transform: uppercase;
        color: var(--muted);
        margin-bottom: 0.5rem;
        padding-left: 0.6rem;
        font-weight: 700;
    }
    .side-link {
        display: block;
        padding: 0.4rem 0.6rem;
        border-left: 2px solid transparent;
        font-size: 0.85rem;
        color: var(--muted);
        transition:
            color 0.15s ease,
            border-color 0.15s ease,
            background 0.15s ease;
    }
    .side-link:hover {
        color: var(--fg);
        background: var(--surface);
    }
    .side-link.active {
        color: var(--brand);
        border-left-color: var(--brand);
        background: var(--surface);
        font-weight: 700;
    }

    .side-foot {
        margin-top: 1.5rem;
        padding: 0.6rem;
        border-top: 2px solid var(--border);
    }

    .docs-main {
        min-width: 0;
        padding: 2rem 0 1rem;
    }
    .docs-breadcrumb {
        margin-bottom: 1.5rem;
        color: var(--muted);
        font-size: 0.7rem;
    }
    .docs-breadcrumb a:hover {
        color: var(--brand);
    }
    .docs-breadcrumb .sep {
        margin: 0 0.5rem;
        opacity: 0.5;
    }

    .docs-pager {
        display: flex;
        justify-content: space-between;
        gap: 1rem;
        margin-top: 4rem;
        padding-top: 1.5rem;
        border-top: 2px solid var(--border);
    }
    .pager-link {
        display: flex;
        flex-direction: column;
        gap: 0.35rem;
        padding: 0.8rem 1rem;
        border: 2px solid var(--border);
        background: var(--bg);
        transition:
            transform 0.08s ease,
            box-shadow 0.08s ease;
        max-width: 48%;
    }
    .pager-link:hover {
        transform: translate(-2px, -2px);
        box-shadow: 4px 4px 0 0 var(--border);
    }
    .pager-link.right {
        text-align: right;
        margin-left: auto;
    }
    .pager-title {
        font-weight: 700;
        color: var(--fg);
        font-size: 0.92rem;
        display: inline-flex;
        align-items: center;
        gap: 0.4rem;
    }
    .pager-link:hover .pager-title {
        color: var(--brand);
    }

    .docs-kicker {
        font-size: 0.7rem;
        letter-spacing: 0.16em;
        text-transform: uppercase;
        color: var(--muted);
        font-weight: 700;
    }

    .docs-outline {
        display: none;
    }
    .outline-link {
        display: block;
        font-size: 0.85rem;
        color: var(--muted);
        padding: 0.22rem 0 0.22rem 0.75rem;
        border-left: 2px solid var(--border);
        transition:
            color 0.15s ease,
            border-color 0.15s ease;
    }
    .outline-link.sub {
        padding-left: 1.4rem;
    }
    .outline-link:hover {
        color: var(--brand);
        border-left-color: var(--brand);
    }

    @media (min-width: 1024px) {
        .docs-layout {
            grid-template-columns: 16rem minmax(0, 1fr);
            gap: 0;
            padding: 0 1.5rem 0 1.5rem;
        }
        .contents-toggle {
            display: none;
        }
        .docs-backdrop {
            display: none;
        }
        .docs-side {
            display: block;
            position: sticky;
            top: 3.5rem;
            align-self: start;
            height: calc(100vh - 3.5rem);
            overflow-y: auto;
            padding: 1.5rem 0.5rem 2rem 0;
        }
        .docs-main {
            padding: 2.5rem 1rem 1rem 2.5rem;
        }
    }

    @media (min-width: 1280px) {
        .docs-layout {
            grid-template-columns: 16rem minmax(0, 1fr) 15rem;
        }
        .docs-outline {
            display: block;
            position: sticky;
            top: 3.5rem;
            align-self: start;
            height: calc(100vh - 3.5rem);
            overflow-y: auto;
            padding: 2.5rem 0 2rem 2rem;
        }
        .docs-main {
            padding-right: 0;
        }
    }
</style>
