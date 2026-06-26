<section class="border-t border-line section-accent">
    <div class="mx-auto max-w-7xl px-5 sm:px-8 py-20 sm:py-28">
        <div class="cta">
            <div class="cta-glow" aria-hidden="true"></div>
            <span class="label relative">Ready to deploy</span>
            <h2
                class="font-display font-extrabold tracking-[-0.03em] text-ink leading-[1.02] text-4xl sm:text-6xl lg:text-7xl mt-6 max-w-2xl relative"
            >
                Your EVM chain,
                <span class="text-gradient">on your terms.</span>
            </h2>
            <p
                class="mt-6 text-muted text-lg sm:text-xl leading-relaxed max-w-xl relative"
            >
                Walrus for data availability, Sui for settlement. Every block is
                independently verifiable. No shared infrastructure, no
                per-transaction fees to a third party, no surprises.
            </p>
            <div class="mt-9 flex flex-wrap items-center gap-3 relative">
                <a href="#/docs/setup" class="btn btn-brand">
                    Start building
                    <svg
                        width="15"
                        height="15"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2.4"
                    >
                        <path d="M5 12h14M13 6l6 6-6 6" />
                    </svg>
                </a>
                <a href="#/docs" class="btn btn-ghost">Read the docs</a>
                <a
                    href="https://github.com/yieldenjoyoors/podseq"
                    target="_blank"
                    rel="noopener"
                    class="btn btn-ghost hidden sm:inline-flex"
                >
                    View on GitHub
                    <svg
                        width="14"
                        height="14"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2"
                    >
                        <path d="M7 17L17 7M9 7h8v8" />
                    </svg>
                </a>
            </div>
            <p class="mt-6 text-sm relative">
                <a
                    href="mailto:hello@podseq.xyz"
                    class="text-brand-ink font-semibold hover:underline underline-offset-4"
                >
                    Need a custom chain? Let's talk.
                </a>
            </p>
        </div>
    </div>
</section>

<style>
    .cta {
        position: relative;
        border: 1px solid rgba(16, 185, 129, 0.2);
        background: var(--color-surface);
        border-radius: 24px;
        padding: 4rem 4.5rem;
        overflow: hidden;
    }
    .cta-glow {
        position: absolute;
        top: -60%;
        right: -20%;
        width: 70%;
        height: 220%;
        background: radial-gradient(
            closest-side,
            rgba(16, 185, 129, 0.12),
            transparent 70%
        );
        pointer-events: none;
    }
    .cta-glow::after {
        content: "";
        position: absolute;
        top: 30%;
        left: -20%;
        width: 50%;
        height: 80%;
        background: radial-gradient(
            closest-side,
            rgba(245, 158, 11, 0.06),
            transparent 65%
        );
        pointer-events: none;
    }
    @media (max-width: 640px) {
        .cta {
            padding: 2.5rem 1.8rem;
            border-radius: 18px;
        }
    }
</style>
