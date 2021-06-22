<script lang="js">
  import Output from '../Output.svelte';

  export let state;
  export let params = {};
</script>

<template>
  {#each $state.data.allRestreams as restream}
    {#if restream.id === params.restream_id}
      {#each restream.outputs as output}
        {#if output.id === params.output_id}
          <Output restream_id={restream.id} value={output} />
        {/if}
      {/each}
    {/if}
  {/each}
</template>
