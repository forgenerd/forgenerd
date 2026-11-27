<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
</script>

<main class="page-loader" aria-busy="true" aria-live="polite">
	<div class="page-loader-content">
		<span class="page-loader-status"><Icon name="progress_activity" size={16} class="animate-spin motion-reduce:animate-none" /> Forging</span>
	</div>
</main>
