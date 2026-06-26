<script lang="ts">
    let open = $state(false);

    const links = [
        { label: "WHY PODSEQ", href: "#why" },
        { label: "HOW IT WORKS", href: "#how" },
        { label: "DOCS", href: "#/docs" },
    ];

    function close() {
        open = false;
    }
</script>

<header
    class="fixed top-0 inset-x-0 z-50 backdrop-blur-xl bg-paper/85 border-b border-line"
>
    <div
        class="mx-auto max-w-7xl px-5 sm:px-8 h-16 flex items-center justify-between gap-6"
    >
        <a href="#hero" class="flex items-center gap-2.5 group" onclick={close}>
            <span class="mark" aria-hidden="true">
                <svg width="22" height="22" viewBox="0 0 22 22" fill="none">
                    <rect
                        x="2"
                        y="3"
                        width="18"
                        height="3"
                        rx="1"
                        fill="currentColor"
                    />
                    <rect
                        x="2"
                        y="9.5"
                        width="13"
                        height="3"
                        rx="1"
                        fill="currentColor"
                        opacity="0.5"
                    />
                    <rect
                        x="2"
                        y="16"
                        width="7"
                        height="3"
                        rx="1"
                        fill="var(--color-brand)"
                    />
                </svg>
            </span>
            <span
                class="font-display font-bold tracking-tight text-ink text-[17px]"
                >podseq</span
            >
        </a>

        <nav class="hidden md:flex items-center gap-1">
            {#each links as link (link.href)}
                <a
                    href={link.href}
                    class="text-sm font-medium text-muted hover:text-ink px-3 py-2 transition-colors"
                >
                    {link.label}
                </a>
            {/each}
        </nav>

        <div class="flex items-center gap-2.5">
            <a
                href="#/docs/setup"
                class="btn btn-brand !py-1.5 !px-3.5 !text-[13px] hidden sm:inline-flex"
            >
                Start building
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
            </a>
            <a
                href="https://github.com/yieldenjoyoors/podseq"
                target="_blank"
                rel="noopener"
                aria-label="GitHub"
                title="GitHub"
                class="w-9 h-9 grid place-items-center rounded-full border border-line text-muted hover:text-ink hover:border-ink/30 transition-colors"
            >
                <svg
                    width="17"
                    height="17"
                    viewBox="0 0 24 24"
                    fill="currentColor"
                >
                    <path
                        fill-rule="evenodd"
                        clip-rule="evenodd"
                        d="M12 2C6.477 2 2 6.477 2 12c0 4.42 2.87 8.17 6.84 9.5.5.08.66-.23.66-.5v-1.69c-2.77.6-3.36-1.34-3.36-1.34-.46-1.16-1.11-1.47-1.11-1.47-.91-.62.07-.6.07-.6 1 .07 1.53 1.03 1.53 1.03.87 1.52 2.34 1.07 2.91.83.09-.65.35-1.09.63-1.34-2.22-.25-4.55-1.11-4.55-4.92 0-1.11.38-2 1.03-2.71-.1-.25-.45-1.29.1-2.64 0 0 .84-.27 2.75 1.02.79-.22 1.65-.33 2.5-.33.85 0 1.71.11 2.5.33 1.91-1.29 2.75-1.02 2.75-1.02.55 1.35.2 2.39.1 2.64.65.71 1.03 1.6 1.03 2.71 0 3.82-2.34 4.66-4.57 4.91.36.31.69.92.69 1.85V21c0 .27.16.59.67.5C19.14 20.16 22 16.42 22 12A10 10 0 0012 2z"
                    />
                </svg>
            </a>
            <button
                class="md:hidden w-9 h-9 grid place-items-center rounded-full border border-line text-ink"
                onclick={() => (open = !open)}
                aria-label="Menu"
            >
                <svg
                    width="18"
                    height="18"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                >
                    {#if open}
                        <path d="M6 6l12 12M18 6L6 18" />
                    {:else}
                        <path d="M3 6h18M3 12h18M3 18h18" />
                    {/if}
                </svg>
            </button>
        </div>
    </div>

    {#if open}
        <div
            class="md:hidden border-t border-line bg-paper/95 px-5 py-3 flex flex-col gap-1"
        >
            {#each links as link (link.href)}
                <a
                    href={link.href}
                    onclick={close}
                    class="text-sm font-medium text-muted hover:text-ink py-2"
                >
                    {link.label}
                </a>
            {/each}
        </div>
    {/if}
</header>

<style>
    .mark {
        color: var(--color-ink);
        transition: color 0.2s ease;
    }
    a.group:hover .mark {
        color: var(--color-brand);
    }
</style>
