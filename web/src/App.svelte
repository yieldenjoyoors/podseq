<script lang="ts">
    import Nav from "./lib/Nav.svelte";
    import Hero from "./lib/Hero.svelte";
    import Stack from "./lib/Stack.svelte";
    import ByDesign from "./lib/ByDesign.svelte";
    import Features from "./lib/Features.svelte";
    import Scenarios from "./lib/Scenarios.svelte";
    import HowItWorks from "./lib/HowItWorks.svelte";
    import Flow from "./lib/Flow.svelte";
    import QuickStart from "./lib/QuickStart.svelte";
    import Faq from "./lib/Faq.svelte";
    import CtaBand from "./lib/CtaBand.svelte";
    import Footer from "./lib/Footer.svelte";
    import Docs from "./lib/Docs.svelte";
    import { DEFAULT_DOC } from "./lib/docs";

    type Route =
        | { kind: "home"; section: string | null }
        | { kind: "docs"; doc: string; anchor: string | null };

    function parse(): Route {
        const hash = location.hash || "#/";
        if (hash.startsWith("#/docs")) {
            const rest = hash.slice("#/docs".length).replace(/^\//, "");
            const [path, anchor] = rest.split("~");
            const doc = path === "" ? DEFAULT_DOC : decodeURIComponent(path);
            return { kind: "docs", doc, anchor: anchor || null };
        }
        if (hash === "#/" || hash === "")
            return { kind: "home", section: null };
        // plain "#section" anchor on the landing page
        const section = hash.replace(/^#\/?/, "");
        return { kind: "home", section: section || null };
    }

    let route = $state<Route>(parse());

    function sync() {
        route = parse();
    }

    $effect(() => {
        window.addEventListener("hashchange", sync);
        return () => window.removeEventListener("hashchange", sync);
    });

    // Scroll to landing section when arriving via a "#section" hash.
    $effect(() => {
        const r = route;
        if (r.kind !== "home" || !r.section) return;
        const id = r.section;
        const go = () => {
            const el = document.getElementById(id);
            if (el) el.scrollIntoView({ behavior: "smooth", block: "start" });
        };
        const raf = requestAnimationFrame(go);
        return () => cancelAnimationFrame(raf);
    });
</script>

<div class="atmosphere"></div>
<div class="bg-dots"></div>
<div class="bg-grain"></div>

<Nav />

{#if route.kind === "docs"}
    <div class="pt-16 min-h-screen">
        <Docs doc={route.doc} anchor={route.anchor} />
    </div>
{:else}
    <main class="pt-16">
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
{/if}

<Footer />
