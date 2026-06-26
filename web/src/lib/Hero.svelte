<script lang="ts">
    const prodBlocks = [0, 1, 2, 3, 4];
    const finBlocks = [0, 1, 2];
</script>

<section
    id="hero"
    class="relative pt-32 sm:pt-40 pb-24 sm:pb-32 overflow-hidden"
>
    <div class="mx-auto max-w-7xl px-5 sm:px-8 pt-14 sm:pt-20">
        <div
            class="grid lg:grid-cols-[1.05fr_0.95fr] gap-14 lg:gap-20 items-center"
        >
            <!-- copy -->
            <div>
                <h1
                    class="reveal font-display font-extrabold tracking-[-0.04em] text-ink leading-[0.94] text-[3.4rem] sm:text-7xl lg:text-[5rem]"
                    style="animation-delay:80ms"
                >
                    Ship your own<br />
                    <span class="text-gradient">EVM chain.</span>
                </h1>

                <p
                    class="reveal mt-8 text-lg sm:text-xl text-muted leading-relaxed max-w-lg"
                    style="animation-delay:180ms"
                >
                    Deploy EVM smart contracts on dedicated infrastructure with
                    verifiable data availability and settlement anchored to Sui.
                    Predictable throughput, no surprises.
                </p>

                <div
                    class="reveal mt-10 flex flex-wrap items-center gap-3"
                    style="animation-delay:260ms"
                >
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
                    <a href="#/docs" class="btn btn-ghost">Explore the docs</a>
                </div>

                <!-- trust badges -->
                <div
                    class="reveal mt-10 flex flex-wrap items-center gap-x-6 gap-y-2"
                    style="animation-delay:320ms"
                >
                    <a
                        href="mailto:hello@podseq.xyz"
                        class="text-sm text-brand-ink font-semibold hover:underline underline-offset-4"
                    >
                        Need a custom chain? Let's talk.
                    </a>
                </div>
            </div>

            <!-- the sequence visual -->
            <div class="reveal" style="animation-delay:200ms">
                <div class="seq-card">
                    <div class="seq-head">
                        <span
                            class="flex items-center gap-2 text-sm font-semibold text-ink"
                        >
                            <span class="dot"></span> sequencer
                        </span>
                        <span
                            class="font-mono text-[10px] tracking-[0.18em] text-brand font-semibold"
                        >
                            LIVE
                        </span>
                    </div>

                    <div class="seq-stages">
                        <span>build</span>
                        <span>p2p</span>
                        <span>walrus</span>
                        <span>sui</span>
                    </div>

                    <div class="seq-lanes">
                        <div class="lane">
                            <div class="lane-label">
                                <span class="lane-title">production</span>
                                <span class="lane-note">sub-second</span>
                            </div>
                            <div class="lane-track">
                                {#each prodBlocks as b (b)}
                                    <span
                                        class="pill pill-prod"
                                        style="animation-delay:{b * 0.75}s"
                                    ></span>
                                {/each}
                            </div>
                        </div>

                        <div class="lane">
                            <div class="lane-label">
                                <span class="lane-title lane-title-2"
                                    >finalization</span
                                >
                                <span class="lane-note">walrus → sui</span>
                            </div>
                            <div class="lane-track">
                                {#each finBlocks as b (b)}
                                    <span
                                        class="pill pill-fin"
                                        style="animation-delay:{b * 1.4}s"
                                    ></span>
                                {/each}
                            </div>
                        </div>
                    </div>

                    <p class="seq-foot">
                        Production runs concurrently with finalization. Block
                        time is never gated by data availability latency.
                    </p>
                </div>
            </div>
        </div>
    </div>
</section>

<style>
    .seq-card {
        background: var(--color-surface);
        border: 1px solid var(--color-line);
        border-radius: 20px;
        padding: 1.2rem 1.4rem 1.3rem;
        box-shadow:
            0 1px 2px rgba(10, 10, 9, 0.03),
            0 32px 64px -32px rgba(16, 185, 129, 0.2);
    }
    .seq-head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        margin-bottom: 1.1rem;
    }

    .seq-stages {
        display: grid;
        grid-template-columns: repeat(4, 1fr);
        gap: 0.5rem;
        padding: 0 0.2rem 0.6rem;
        margin-bottom: 0.5rem;
    }
    .seq-stages span {
        font-family: var(--font-mono);
        font-size: 0.6rem;
        letter-spacing: 0.16em;
        text-transform: uppercase;
        color: var(--color-faint);
    }

    .seq-lanes {
        display: flex;
        flex-direction: column;
        gap: 1rem;
    }
    .lane-label {
        display: flex;
        justify-content: space-between;
        align-items: baseline;
        margin-bottom: 0.5rem;
    }
    .lane-title {
        font-family: var(--font-sans);
        font-size: 0.8rem;
        font-weight: 600;
        color: var(--color-brand-ink);
    }
    .lane-title-2 {
        color: var(--color-muted);
    }
    .lane-note {
        font-family: var(--font-mono);
        font-size: 0.6rem;
        color: var(--color-faint);
    }
    .lane-track {
        position: relative;
        height: 18px;
        background: var(--color-surface-2);
        border-radius: 999px;
        overflow: hidden;
    }
    /* stage dividers */
    .lane-track::before {
        content: "";
        position: absolute;
        inset: 0;
        background-image:
            linear-gradient(
                90deg,
                transparent 24.5%,
                var(--color-line) 25%,
                transparent 25.5%
            ),
            linear-gradient(
                90deg,
                transparent 49.5%,
                var(--color-line) 50%,
                transparent 50.5%
            ),
            linear-gradient(
                90deg,
                transparent 74.5%,
                var(--color-line) 75%,
                transparent 75.5%
            );
        opacity: 0.6;
    }

    .pill {
        position: absolute;
        top: 4px;
        left: -10%;
        height: 10px;
        border-radius: 999px;
        opacity: 0;
    }
    .pill-prod {
        width: 44px;
        background: linear-gradient(
            90deg,
            var(--color-brand-2),
            var(--color-brand)
        );
        box-shadow: 0 0 8px -1px rgba(16, 185, 129, 0.5);
        animation: seq-prod 3.2s cubic-bezier(0.4, 0, 0.2, 1) infinite;
    }
    .pill-fin {
        width: 34px;
        background: #a8a29e;
        box-shadow: 0 0 6px -1px rgba(120, 113, 108, 0.4);
        animation: seq-fin 6.4s cubic-bezier(0.4, 0, 0.2, 1) infinite;
    }

    @keyframes seq-prod {
        0% {
            left: -10%;
            opacity: 0;
        }
        10% {
            opacity: 1;
        }
        55% {
            left: 50%;
            opacity: 1;
        }
        72% {
            left: 52%;
            opacity: 0.9;
        }
        100% {
            left: 52%;
            opacity: 0;
        }
    }
    @keyframes seq-fin {
        0% {
            left: -10%;
            opacity: 0;
        }
        8% {
            opacity: 1;
        }
        92% {
            opacity: 1;
        }
        100% {
            left: 104%;
            opacity: 0;
        }
    }

    .seq-foot {
        margin-top: 1.2rem;
        padding-top: 0.9rem;
        border-top: 1px solid var(--color-line);
        font-size: 0.72rem;
        line-height: 1.5;
        color: var(--color-faint);
    }

    @media (prefers-reduced-motion: reduce) {
        .pill-prod,
        .pill-fin {
            animation: none;
            opacity: 1;
        }
        .pill-prod {
            left: 30%;
        }
        .pill-fin {
            left: 65%;
        }
    }
</style>
