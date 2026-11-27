<script lang="ts">
	import type { Snippet } from 'svelte';

	let {
		open = $bindable(false),
		title,
		confirmLabel,
		danger = false,
		busy = false,
		onconfirm,
		children
	}: {
		open?: boolean;
		title: string;
		confirmLabel: string;
		danger?: boolean;
		busy?: boolean;
		onconfirm: () => void;
		children?: Snippet;
	} = $props();

	let dialog: HTMLDialogElement | undefined = $state();
	$effect(() => {
		if (!dialog) return;
		if (open && !dialog.open) dialog.showModal();
		if (!open && dialog.open) dialog.close();
	});
</script>

<dialog
	bind:this={dialog}
	onclose={() => (open = false)}
	class="m-auto w-full max-w-md rounded-xl border border-line bg-surface p-0 text-text shadow-[var(--shadow)] backdrop:bg-black/60"
>
	<div class="p-5">
		<h2 class="text-base font-medium">{title}</h2>
		<div class="mt-2 text-[13px] text-muted">{@render children?.()}</div>
	</div>
	<div class="flex justify-end gap-2 border-t border-line px-5 py-3">
		<button class="btn" onclick={() => (open = false)}>Cancel</button>
		<button class="btn {danger ? 'btn-danger' : 'btn-primary'}" disabled={busy} onclick={onconfirm}>{confirmLabel}</button>
	</div>
</dialog>
