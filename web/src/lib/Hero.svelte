<script lang="ts">
    const prodBlocks = [0, 1, 2, 3, 4];
    const finBlocks = [0, 1, 2];
</script>

<section id="hero" class="border-b-2 border-[var(--border)]">
    <div
        class="mx-auto grid max-w-5xl grid-cols-1 gap-12 px-4 py-16 lg:grid-cols-2 lg:items-center lg:py-24"
    >
        <!-- copy -->
        <div>
            <h1
                class="reveal text-4xl font-extrabold uppercase leading-[0.95] tracking-tight md:text-6xl"
                style="animation-delay:60ms"
            >
                Ship your own<br />
                <span class="text-[var(--brand)]">EVM chain.</span>
            </h1>

            <p
                class="reveal mt-6 max-w-md body-sm leading-relaxed text-[var(--muted)]"
                style="animation-delay:140ms"
            >
                Deploy EVM smart contracts on a chain you own. Walrus
                handles data availability, Sui handles settlement, and
                throughput stays predictable.
            </p>

            <div
                class="reveal mt-8 flex flex-col gap-3 sm:flex-row"
                style="animation-delay:220ms"
            >
                <a href="#/docs/setup" class="btn btn-brand">
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
                <a href="#/docs" class="btn">Explore the docs</a>
            </div>

            <div
                class="reveal mt-8 flex flex-wrap items-center gap-x-6 gap-y-2"
                style="animation-delay:280ms"
            >
                <a
                    href="mailto:hello@podseq.xyz"
                    class="micro font-bold uppercase tracking-wide text-[var(--brand)] underline underline-offset-4 hover:no-underline"
                >
                    Need a custom chain? Let's talk.
                </a>
            </div>
        </div>

        <!-- the sequence visual -->
        <div class="reveal" style="animation-delay:180ms">
            <div class="seq-card">
                <div class="seq-head">
                    <span class="flex items-center gap-2 micro font-bold">
                        <span class="dot"></span> sequencer
                    </span>
                    <span class="kicker text-[var(--brand)]" style="letter-spacing: 0.16em">Live</span>
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
                    time is never gated by DA latency.
                </p>
            </div>
        </div>
    </div>
</section>

<style>
    .seq-card {
        background: var(--bg);
        border: 2px solid var(--border);
        box-shadow: 6px 6px 0 0 var(--border);
        padding: 1.2rem 1.4rem 1.3rem;
    }
    .seq-head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        margin-bottom: 1.1rem;
        padding-bottom: 0.9rem;
        border-bottom: 2px solid var(--border);
    }

    .seq-stages {
        display: grid;
        grid-template-columns: repeat(4, 1fr);
        gap: 0.5rem;
        padding: 0 0.2rem 0.6rem;
        margin-bottom: 0.5rem;
    }
    .seq-stages span {
        font-size: 0.7rem;
        letter-spacing: 0.16em;
        text-transform: uppercase;
        color: var(--faint);
        font-weight: 700;
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
        font-size: 0.7rem;
        font-weight: 700;
        text-transform: uppercase;
        letter-spacing: 0.02em;
        color: var(--brand);
    }
    .lane-title-2 {
        color: var(--muted);
    }
    .lane-note {
        font-size: 0.7rem;
        color: var(--faint);
        font-weight: 600;
    }
    .lane-track {
        position: relative;
        height: 18px;
        background: var(--surface);
        border: 1.5px solid var(--border);
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
                var(--border) 25%,
                transparent 25.5%
            ),
            linear-gradient(
                90deg,
                transparent 49.5%,
                var(--border) 50%,
                transparent 50.5%
            ),
            linear-gradient(
                90deg,
                transparent 74.5%,
                var(--border) 75%,
                transparent 75.5%
            );
        opacity: 0.4;
    }

    .pill {
        position: absolute;
        top: 3px;
        left: -10%;
        height: 10px;
        opacity: 0;
    }
    .pill-prod {
        width: 44px;
        background: var(--brand);
        animation: seq-prod 3.2s cubic-bezier(0.4, 0, 0.2, 1) infinite;
    }
    .pill-fin {
        width: 34px;
        background: var(--fg);
        opacity: 0.6;
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
            opacity: 0.6;
        }
        92% {
            opacity: 0.6;
        }
        100% {
            left: 104%;
            opacity: 0;
        }
    }

    .seq-foot {
        margin-top: 1.2rem;
        padding-top: 0.9rem;
        border-top: 1.5px solid var(--border);
        font-size: 0.85rem;
        line-height: 1.5;
        color: var(--muted);
    }

    @media (prefers-reduced-motion: reduce) {
        .pill-prod,
        .pill-fin {
            animation: none;
        }
        .pill-prod {
            left: 30%;
            opacity: 1;
        }
        .pill-fin {
            left: 65%;
            opacity: 0.6;
        }
    }
</style>
