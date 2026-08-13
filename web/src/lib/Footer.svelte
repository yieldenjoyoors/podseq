<script lang="ts">
    type Link = { label: string; href: string; external?: boolean };
    type Column = { title: string; links: Link[] };

    const cols: Column[] = [
        {
            title: "Docs",
            links: [
                { label: "Architecture", href: "#/docs/architecture" },
                { label: "Node setup", href: "#/docs/setup" },
                { label: "Block production", href: "#/docs/block-production" },
            ],
        },
        {
            title: "Built on",
            links: [
                { label: "Reth", href: "https://reth.rs", external: true },
                {
                    label: "Walrus",
                    href: "https://docs.wal.app",
                    external: true,
                },
                { label: "Sui", href: "https://sui.io", external: true },
            ],
        },
        {
            title: "Social",
            links: [
                {
                    label: "GitHub",
                    href: "https://github.com/yieldenjoyoors/podseq",
                    external: true,
                },
            ],
        },
    ];
</script>

<footer class="border-t-2 border-[var(--border)]">
    <div class="mx-auto max-w-5xl px-4 py-12">
        <div class="grid grid-cols-2 md:grid-cols-5 gap-8">
            <div class="col-span-2">
                <div class="flex items-center gap-2">
                    <svg width="22" height="22" viewBox="0 0 22 22" fill="none">
                        <rect x="0" y="0" width="22" height="22" fill="#0a0a0a" />
                        <rect x="2" y="3" width="18" height="3" fill="#fafaf7" />
                        <rect
                            x="2"
                            y="9.5"
                            width="13"
                            height="3"
                            fill="#fafaf7"
                            opacity="0.5"
                        />
                        <rect x="2" y="16" width="7" height="3" fill="#e85f00" />
                    </svg>
                    <span class="font-extrabold uppercase tracking-tight text-lg"
                        >podseq</span
                    >
                </div>
                <p class="mt-4 micro leading-relaxed text-[var(--muted)] max-w-xs">
                    Deploy your own EVM L2 on Sui.
                </p>
            </div>

            {#each cols as col (col.title)}
                <div>
                    <h4 class="kicker text-[var(--muted)] mb-3">{col.title}</h4>
                    <ul class="space-y-1.5">
                        {#each col.links as link (link.href)}
                            <li>
                                <a
                                    href={link.href}
                                    target={link.external
                                        ? "_blank"
                                        : undefined}
                                    rel={link.external ? "noopener" : undefined}
                                    class="micro font-bold uppercase tracking-wide text-[var(--fg)] hover:text-[var(--brand)] transition-colors"
                                >
                                    {link.label}
                                </a>
                            </li>
                        {/each}
                    </ul>
                </div>
            {/each}
        </div>
    </div>
    <div class="border-t-2 border-[var(--border)]">
        <div
            class="mx-auto flex max-w-5xl items-center justify-center px-4 py-3 micro uppercase tracking-wide text-[var(--muted)]"
        >
            <span
                >© {new Date().getFullYear()}
                <a
                    href="https://yieldenjoyoors.com"
                    target="_blank"
                    rel="noopener"
                    class="hover:text-[var(--fg)]"
                >
                    Yield Enjoyoors B.V.
                </a></span
            >
        </div>
    </div>
</footer>
