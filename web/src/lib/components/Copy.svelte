<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';

	let { value, label = 'Copy' }: { value: string; label?: string } = $props();
	let copied = $state(false);

	async function copy() {
		try {
			await navigator.clipboard.writeText(value);
		} catch {
			// Clipboard API needs a secure context; fall back to a hidden textarea.
			const ta = document.createElement('textarea');
			ta.value = value;
			document.body.append(ta);
			ta.select();
			document.execCommand('copy');
			ta.remove();
		}
		copied = true;
		setTimeout(() => (copied = false), 1500);
	}
</script>

<button type="button" class="btn btn-ghost btn-sm" onclick={copy} aria-label={label} title={label}>
	{#if copied}<Icon name="check" size={17} class="text-ok" />{:else}<Icon name="content_copy" size={17} />{/if}
</button>
