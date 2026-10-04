<script lang="ts">
    import DocPage from "#lib/DocPage.svelte";
    import type { PageProps } from "./$types";

    let { data }: PageProps = $props();
</script>

<div class="pt-14 min-h-screen">
    <DocPage
        slug={data.slug}
        title={data.title}
        description={data.description}
        rendered={data.rendered}
    />
</div>
