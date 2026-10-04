<script lang="ts">
    import "../app.css";
    import Footer from "#lib/Footer.svelte";
    import Nav from "#lib/Nav.svelte";

    let { children } = $props();
</script>

<Nav />

{@render children()}

<Footer />
