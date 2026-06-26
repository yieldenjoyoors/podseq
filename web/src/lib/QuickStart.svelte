<script lang="ts">
    type Tab = { id: string; label: string; code: string };

    let tabs: Tab[] = [
        {
            id: "jwt",
            label: "jwt",
            code: `# shared 32-byte secret for Reth + podseq
head -c 32 /dev/urandom | od -A n -t x1 | tr -d ' \\n' > jwt.hex

# reth
reth node --authrpc.jwtsecret jwt.hex

# both sides must use the same jwt.hex`,
        },
        {
            id: "keys",
            label: "keys",
            code: `# settlement key (Sui suiprivkey, ed25519)
sui keytool generate ed25519    # save suiprivkey... to sui.key

# block signing key
podseq keyring generate-block --out block.key
podseq keyring list`,
        },
        {
            id: "config",
            label: "config",
            code: `podseq init config --out podseq.toml

[reth]
engine_url = "http://localhost:8551"
jwt_path   = "jwt.hex"

[walrus]
publisher_url  = "https://publisher.walrus-testnet.walrus.space"
aggregator_url = "https://aggregator.walrus-testnet.walrus.space"

[sui]
rpc_url = "https://fullnode.testnet.sui.io:443"

[signer]
block_key_path      = "block.key"
settlement_key_path = "sui.key"`,
        },
        {
            id: "run",
            label: "run",
            code: `cargo build --release

# sequencer (default): deploys settlement on first start
podseq start --config podseq.toml

# or a full node syncing from DA + settlement
podseq start --config podseq.toml --mode full`,
        },
    ];

    let active = $state(tabs[0].id);
    let copied = $state(false);
    const current = $derived(tabs.find((t) => t.id === active) ?? tabs[0]);

    async function copy() {
        try {
            await navigator.clipboard.writeText(current.code);
            copied = true;
            window.setTimeout(() => (copied = false), 1400);
        } catch {
            // clipboard unavailable
        }
    }
</script>

<section class="py-12 sm:py-16">
    <div class="mx-auto max-w-7xl px-5 sm:px-8">
        <div class="grid lg:grid-cols-[0.9fr_1.1fr] gap-12 items-start">
            <div class="lg:sticky lg:top-24">
                <span class="label">Quick start</span>
                <h2
                    class="font-display font-bold tracking-tight text-3xl sm:text-4xl text-ink mt-5"
                >
                    A running node in four commands.
                </h2>
                <p class="mt-4 text-muted text-lg leading-relaxed">
                    Create your keys, write a minimal config, and start.
                    Everything else defaults to testnet.
                </p>
                <a href="#/docs/setup" class="btn btn-ghost mt-7">
                    Full setup guide
                    <svg
                        width="14"
                        height="14"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2.4"
                    >
                        <path d="M5 12h14M13 6l6 6-6 6" />
                    </svg>
                </a>
            </div>

            <div class="terminal">
                <div class="term-tabs">
                    {#each tabs as tab (tab.id)}
                        <button
                            class="term-tab"
                            class:active={tab.id === active}
                            onclick={() => (active = tab.id)}
                        >
                            {tab.label}
                        </button>
                    {/each}
                    <button class="copy" onclick={copy} title="Copy">
                        {#if copied}
                            <svg
                                width="14"
                                height="14"
                                viewBox="0 0 24 24"
                                fill="none"
                                stroke="currentColor"
                                stroke-width="2.4"
                                class="text-[#10b981]"
                            >
                                <path d="M20 6L9 17l-5-5" />
                            </svg>
                        {:else}
                            <svg
                                width="14"
                                height="14"
                                viewBox="0 0 24 24"
                                fill="none"
                                stroke="currentColor"
                                stroke-width="2"
                            >
                                <rect
                                    x="9"
                                    y="9"
                                    width="11"
                                    height="11"
                                    rx="1.5"
                                />
                                <path d="M5 15V5a1 1 0 0 1 1-1h10" />
                            </svg>
                        {/if}
                        <span>{copied ? "copied" : "copy"}</span>
                    </button>
                </div>
                <pre class="term-body"><code>{current.code}</code></pre>
            </div>
        </div>
    </div>
</section>

<style>
    .terminal {
        border: 1px solid #20242b;
        border-radius: 14px;
        overflow: hidden;
        background: #0c0e11;
        box-shadow: 0 28px 60px -34px rgba(23, 23, 29, 0.5);
    }
    .term-tabs {
        display: flex;
        align-items: center;
        border-bottom: 1px solid #20242b;
        background: #0a0c0f;
    }
    .term-tab {
        font-family: var(--font-mono);
        font-size: 0.78rem;
        color: #8c8c95;
        padding: 0.7rem 1rem;
        border-right: 1px solid #20242b;
        cursor: pointer;
        transition:
            color 0.15s ease,
            background 0.15s ease;
    }
    .term-tab:hover {
        color: #f4f2ec;
    }
    .term-tab.active {
        color: #10b981;
        background: #0c0e11;
        box-shadow: inset 0 -2px 0 #10b981;
    }
    .copy {
        margin-left: auto;
        display: inline-flex;
        align-items: center;
        gap: 0.4rem;
        font-family: var(--font-mono);
        font-size: 0.72rem;
        color: #8c8c95;
        padding: 0.7rem 0.9rem;
        cursor: pointer;
        transition: color 0.15s ease;
    }
    .copy:hover {
        color: #10b981;
    }
    .term-body {
        margin: 0;
        padding: 1.2rem 1.3rem;
        overflow-x: auto;
        font-family: var(--font-mono);
        font-size: 0.82rem;
        line-height: 1.7;
        color: #d8d4ca;
        background: #0c0e11;
    }
    .term-body code {
        white-space: pre;
        color: inherit;
        background: none;
        border: none;
        padding: 0;
    }
</style>
