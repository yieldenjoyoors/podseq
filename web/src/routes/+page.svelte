<script lang="ts">
    import ByDesign from "#lib/ByDesign.svelte";
    import CtaBand from "#lib/CtaBand.svelte";
    import Faq from "#lib/Faq.svelte";
    import Features from "#lib/Features.svelte";
    import Flow from "#lib/Flow.svelte";
    import Hero from "#lib/Hero.svelte";
    import HowItWorks from "#lib/HowItWorks.svelte";
    import QuickStart from "#lib/QuickStart.svelte";
    import Scenarios from "#lib/Scenarios.svelte";
    import Stack from "#lib/Stack.svelte";
</script>

<svelte:head>
    <title>Podseq: Deploy your own EVM L2</title>
    <meta
        name="description"
        content="Podseq: deploy your own EVM L2. Reth execution, Walrus data availability, Sui settlement. Dedicated throughput, predictable costs, every block verifiable."
    />
</svelte:head>

<main class="pt-14">
    <Hero />
    <Stack />
    <ByDesign />
    <Features />
    <Scenarios />
    <HowItWorks />
    <Flow />

    <QuickStart />

    <Faq />
    <CtaBand />
</main>
