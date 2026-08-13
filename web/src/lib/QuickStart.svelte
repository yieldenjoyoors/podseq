<script lang="ts">
    import { highlightCode } from "./docs";

    type Tab = { id: string; label: string; lang: string; code: string };

    let tabs: Tab[] = [
        {
            id: "jwt",
            label: "jwt",
            lang: "sh",
            code: `# shared 32-byte secret for Reth + podseq
head -c 32 /dev/urandom | od -A n -t x1 | tr -d ' \\n' > jwt.hex

# reth
reth node --authrpc.jwtsecret jwt.hex

# both sides must use the same jwt.hex`,
        },
        {
            id: "keys",
            label: "keys",
            lang: "sh",
            code: `# settlement key (Sui suiprivkey, ed25519)
sui keytool generate ed25519    # save suiprivkey... to sui.key

# block signing key
podseq keyring generate-block --out block.key
podseq keyring list`,
        },
        {
            id: "config",
            label: "config",
            lang: "toml",
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
            lang: "sh",
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
    const html = $derived(highlightCode(current.code, current.lang));

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

<section class="border-b-2 border-[var(--border)] py-12">
    <div class="mx-auto max-w-5xl px-4">
        <div class="grid lg:grid-cols-[0.9fr_1.1fr] gap-10 items-start">
            <div class="lg:sticky lg:top-24">
                <span class="label">Quick start</span>
                <h2
                    class="mt-2 text-2xl font-extrabold uppercase tracking-tight md:text-3xl"
                >
                    A running node in four commands.
                </h2>
                <p class="mt-4 body-sm leading-relaxed text-[var(--muted)]">
                    Create your keys, write a minimal config, and start.
                    Everything else defaults to testnet.
                </p>
                <a href="#/docs/setup" class="btn mt-6">
                    Full setup guide
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
                                class="text-[#f59a52]"
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
                                />
                                <path d="M5 15V5a1 1 0 0 1 1-1h10" />
                            </svg>
                        {/if}
                        <span>{copied ? "copied" : "copy"}</span>
                    </button>
                </div>
                <pre class="term-body"><code>{@html html}</code></pre>
            </div>
        </div>
    </div>
</section>

<style>
    .terminal {
        border: 2px solid var(--border);
        background: #0a0a0a;
        box-shadow: 6px 6px 0 0 var(--border);
    }
    .term-tabs {
        display: flex;
        align-items: center;
        border-bottom: 2px solid var(--border);
        background: #161616;
    }
    .term-tab {
        font-size: 0.7rem;
        font-weight: 700;
        text-transform: uppercase;
        letter-spacing: 0.04em;
        color: #9a9a9a;
        padding: 0.7rem 1rem;
        border-right: 1px solid #3a3a3a;
        cursor: pointer;
        transition:
            color 0.15s ease,
            background 0.15s ease;
    }
    .term-tab:hover {
        color: #f5f5ef;
    }
    .term-tab.active {
        color: #f59a52;
        background: #0a0a0a;
        box-shadow: inset 0 -2px 0 #f59a52;
    }
    .copy {
        margin-left: auto;
        display: inline-flex;
        align-items: center;
        gap: 0.4rem;
        font-size: 0.7rem;
        font-weight: 700;
        text-transform: uppercase;
        letter-spacing: 0.04em;
        color: #9a9a9a;
        padding: 0.7rem 0.9rem;
        cursor: pointer;
        transition: color 0.15s ease;
        background: none;
        border: none;
    }
    .copy:hover {
        color: #f59a52;
    }
    .term-body {
        margin: 0;
        padding: 1.2rem 1.3rem;
        overflow-x: auto;
        font-size: 0.85rem;
        line-height: 1.7;
        color: #d8d8d2;
        background: #0a0a0a;
    }
    .term-body code {
        white-space: pre;
        color: #d8d8d2;
        background: none;
        border: none;
        padding: 0;
    }
    .term-body :global(.hljs-comment),
    .term-body :global(.hljs-quote) {
        color: #6b6b6b;
        font-style: italic;
    }
    .term-body :global(.hljs-string),
    .term-body :global(.hljs-attr),
    .term-body :global(.hljs-template-tag),
    .term-body :global(.hljs-template-variable),
    .term-body :global(.hljs-addition) {
        color: #e5a663;
    }
    .term-body :global(.hljs-number),
    .term-body :global(.hljs-built_in),
    .term-body :global(.hljs-type),
    .term-body :global(.hljs-boolean) {
        color: #d19a66;
    }
    .term-body :global(.hljs-keyword),
    .term-body :global(.hljs-literal),
    .term-body :global(.hljs-section),
    .term-body :global(.hljs-link) {
        color: #f59a52;
        font-weight: 600;
    }
    .term-body :global(.hljs-title),
    .term-body :global(.hljs-title.function_),
    .term-body :global(.hljs-name) {
        color: #61afef;
    }
    .term-body :global(.hljs-variable),
    .term-body :global(.hljs-property) {
        color: #e06c75;
    }
    .term-body :global(.hljs-symbol),
    .term-body :global(.hljs-bullet),
    .term-body :global(.hljs-meta) {
        color: #c678dd;
    }
    .term-body :global(.hljs-emphasis) {
        font-style: italic;
    }
    .term-body :global(.hljs-strong) {
        font-weight: 700;
    }
</style>
