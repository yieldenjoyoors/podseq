<script lang="ts">
    const scenarios = [
        {
            tag: "Extend",
            title: "Bring EVM to Sui",
            body: "Run EVM smart contracts that settle on Sui, with Walrus for data availability. Your existing contracts deploy unchanged.",
        },
        {
            tag: "Connect",
            title: "Tap into Sui DeFi",
            body: "Your users keep the EVM tooling they know, and get access to Sui liquidity and DeFi protocols.",
        },
        {
            tag: "Comply",
            title: "Auditable, policy-ready chains",
            body: "Deterministic EVM execution with verifiable commitments on Sui. Built for payments and regulated workloads that need an audit trail.",
        },
    ];
</script>

<section id="scenarios" class="border-b-2 border-[var(--border)] py-16">
    <div class="mx-auto max-w-5xl px-4">
        <div class="max-w-2xl mb-10">
            <span class="label">When to run your own chain</span>
            <h2
                class="mt-2 text-2xl font-extrabold uppercase tracking-tight md:text-3xl"
            >
                Three signals it's time.
            </h2>
            <p class="mt-4 body-sm leading-relaxed text-[var(--muted)]">
                Not every project needs dedicated infrastructure. These are the
                cases where it pays for itself.
            </p>
        </div>

        <div class="scenarios-grid">
            {#each scenarios as s, i (s.tag)}
                <div class="scenario">
                    <span class="chip">{i + 1}. {s.tag}</span>
                    <h3 class="scenario-title">{s.title}</h3>
                    <p class="scenario-body">{s.body}</p>
                </div>
            {/each}
        </div>
    </div>
</section>

<style>
    .scenarios-grid {
        display: grid;
        grid-template-columns: 1fr;
        border: 2px solid var(--border);
        background: var(--border);
        gap: 2px;
    }
    @media (min-width: 768px) {
        .scenarios-grid {
            grid-template-columns: repeat(3, 1fr);
        }
    }
    .scenario {
        padding: 1.75rem;
        background: var(--bg);
    }
    .scenario-title {
        font-weight: 700;
        font-size: 1.1rem;
        text-transform: uppercase;
        letter-spacing: -0.01em;
        margin: 1rem 0 0.6rem;
    }
    .scenario-body {
        font-size: 0.85rem;
        line-height: 1.6;
        color: var(--muted);
    }
</style>
