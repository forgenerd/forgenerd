<script lang="ts">
	import { get, type AppDetail, type ContainerStats } from '#lib/api.ts';
	import { bytes } from '#lib/format.ts';

	let { app }: { app: AppDetail } = $props();
	let stats = $state<ContainerStats[] | null>(null);

	async function load(id: string) {
		try {
			stats = await get<ContainerStats[]>(`/apps/${id}/stats`);
		} catch {
			stats = null;
		}
	}

	// Sampling takes about a second on the server, so poll gently and only while running.
	$effect(() => {
		const id = app.id;
		if (app.status !== 'running' && app.status !== 'unhealthy') {
			stats = null;
			return;
		}
		load(id);
		const timer = setInterval(() => load(id), 15_000);
		return () => clearInterval(timer);
	});

	const cpu = $derived(stats?.reduce((sum, s) => sum + s.cpu_percent, 0) ?? 0);
	const memory = $derived(stats?.reduce((sum, s) => sum + s.memory_bytes, 0) ?? 0);
	const limit = $derived(app.memory_limit_mb ? app.memory_limit_mb * 1024 * 1024 : null);
	const nearLimit = $derived(limit !== null && memory / limit > 0.9);
</script>

{#if stats && stats.length}
	<span class="flex items-center gap-1.5" title={stats.length > 1 ? `Total of ${stats.length} containers` : 'Current usage'}>
		<span>CPU <span class="text-text tabular-nums">{cpu.toFixed(1)}%</span></span>
		<span aria-hidden="true">·</span>
		<span class={nearLimit ? 'text-warn' : ''}>
			Memory <span class="tabular-nums {nearLimit ? '' : 'text-text'}">{bytes(memory)}</span>{#if limit} of {bytes(limit)}{/if}
		</span>
	</span>
{/if}
