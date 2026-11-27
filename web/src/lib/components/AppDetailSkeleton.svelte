<script lang="ts">
	import Skeleton from '#lib/components/Skeleton.svelte';
</script>

<div aria-busy="true" aria-label="Loading app" class="mt-2">
	<div class="flex flex-wrap items-start justify-between gap-4">
		<div class="grid gap-3">
			<div class="flex items-center gap-3"><Skeleton class="h-6 w-32" /><Skeleton class="h-5 w-20 rounded-full" /></div>
			<Skeleton class="h-3 w-72 max-w-[75vw]" />
		</div>
		<div class="flex gap-2"><Skeleton class="h-9 w-24" /><Skeleton class="h-9 w-20" /></div>
	</div>
	<div class="mt-6 flex gap-1 border-b border-line">
		{#each Array(5) as _, i (i)}<Skeleton class="my-3 h-4 w-24" />{/each}
	</div>
	<div class="panel mt-6 overflow-hidden">
		<div class="border-b border-line px-5 py-4"><Skeleton class="h-4 w-36" /></div>
		{#each Array(4) as _, i (i)}
			<div class="flex items-center gap-4 border-b border-line px-5 py-4 last:border-b-0">
				<Skeleton class="h-4 w-20" />
				<Skeleton class="h-3 w-28" />
				<Skeleton class="h-3 w-36 max-w-[35%]" />
			</div>
		{/each}
	</div>
</div>
