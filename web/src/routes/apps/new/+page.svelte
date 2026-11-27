<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';

	onMount(() => goto('/'));
</script>

<div class="page-loader" aria-busy="true">Opening app creator…</div>
