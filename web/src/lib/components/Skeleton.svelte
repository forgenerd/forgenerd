<script lang="ts">
	let { class: className = '', label = 'Loading' }: { class?: string; label?: string } = $props();
</script>

<span class="skeleton {className}" role="status" aria-label={label}></span>
