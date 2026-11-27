<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import Copy from '#lib/components/Copy.svelte';
	import LogView from '#lib/components/LogView.svelte';
	import Status from '#lib/components/Status.svelte';
	import {
		get,
		patch,
		post,
		queueRedeploy,
		errorMessage,
		type AppDetail,
		type ComposeService
	} from '#lib/api.ts';
	import { notify } from '#lib/toast.svelte.ts';

	let { app, onchange }: { app: AppDetail; onchange?: () => void | Promise<void> } = $props();
	let services = $state<ComposeService[]>([]);
	let loading = $state(true);
	let error = $state('');
	let selectedName = $state<string | null>(null);
	let view = $state<'overview' | 'environment' | 'logs'>('overview');
	let revealed = $state(false);
	let editing = $state(false);
	let editingConfig = $state(false);
	let envDraft = $state<{ key: string; value: string }[]>([]);
	let configDraft = $state<{ ports: { value: string }[]; volumes: { value: string }[] }>({
		ports: [],
		volumes: []
	});
	let busy = $state<string | null>(null);

	const views = [
		{ id: 'overview', label: 'Overview' },
		{ id: 'environment', label: 'Environment' },
		{ id: 'logs', label: 'Logs' }
	] as const;
	const selected = $derived(services.find((s) => s.name === selectedName) ?? services[0]);

	async function load() {
		if (editing || editingConfig) return;
		try {
			// Public service first; the rest keep the server's order.
			services = (await get<ComposeService[]>(`/apps/${app.id}/services`)).sort((a, b) => Number(b.exposed) - Number(a.exposed));
			error = '';
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		app.id;
		load();
		const timer = setInterval(load, 5000);
		return () => clearInterval(timer);
	});

	function status(service: ComposeService): string {
		if (!service.present) return 'not_deployed';
		if (service.restarting) return 'restarting';
		if (!service.running) return 'stopped';
		if (service.health === 'unhealthy') return 'unhealthy';
		return service.health === 'healthy' ? 'healthy' : 'running';
	}

	const dot: Record<string, string> = {
		healthy: 'bg-ok',
		running: 'bg-ok',
		restarting: 'bg-warn',
		unhealthy: 'bg-bad',
		stopped: 'bg-line-strong',
		not_deployed: 'border border-line-strong'
	};

	function select(name: string) {
		if (name === selected?.name) return;
		cancelEdit();
		revealed = false;
		selectedName = name;
	}

	function startEdit(service: ComposeService) {
		envDraft = service.environment.map((e) => ({ key: e.key, value: e.value ?? '' }));
		if (envDraft.length === 0) envDraft.push({ key: '', value: '' });
		editing = true;
	}

	function startConfigEdit(service: ComposeService) {
		configDraft = {
			ports: service.ports.map((value) => ({ value })),
			volumes: service.volumes.map((value) => ({ value }))
		};
		editingConfig = true;
	}

	function cancelEdit() {
		editing = false;
		editingConfig = false;
		envDraft = [];
		configDraft = { ports: [], volumes: [] };
	}

	function normalizedEnvironment(entries: { key: string; value: string | null }[]) {
		return entries
			.map((entry) => ({ key: entry.key.trim(), value: entry.value ?? '' }))
			.filter((entry) => entry.key)
			.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
	}

	function sameEnvironment(left: { key: string; value: string | null }[], right: { key: string; value: string | null }[]) {
		return JSON.stringify(normalizedEnvironment(left)) === JSON.stringify(normalizedEnvironment(right));
	}

	function normalizedList(entries: string[]) {
		return entries.map((entry) => entry.trim()).filter(Boolean);
	}

	async function finishSave(service: ComposeService, result: ComposeService & { redeploy_required: boolean }, message: string) {
		await queueRedeploy(app.id, result.redeploy_required);
		services = services.map((item) => (item.name === service.name ? result : item));
		cancelEdit();
		await onchange?.();
		notify(result.redeploy_required ? `${message} Restart queued.` : `${message}.`);
	}

	async function saveEnvironment(service: ComposeService) {
		const environment = envDraft
			.filter((e) => e.key.trim())
			.map((e) => ({ key: e.key.trim(), value: e.value }));
		if (sameEnvironment(service.environment, environment)) {
			cancelEdit();
			return;
		}
		busy = 'environment';
		try {
			const result = await patch<ComposeService & { redeploy_required: boolean }>(
				`/apps/${app.id}/services/${encodeURIComponent(service.name)}`,
				{ environment }
			);
			await finishSave(service, result, 'Environment saved');
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			busy = null;
		}
	}

	async function saveServiceConfig(service: ComposeService) {
		const ports = normalizedList(configDraft.ports.map((entry) => entry.value));
		const volumes = normalizedList(configDraft.volumes.map((entry) => entry.value));
		if (JSON.stringify(ports) === JSON.stringify(normalizedList(service.ports)) && JSON.stringify(volumes) === JSON.stringify(normalizedList(service.volumes))) {
			cancelEdit();
			return;
		}
		busy = 'config';
		try {
			const result = await patch<ComposeService & { redeploy_required: boolean }>(
				`/apps/${app.id}/services/${encodeURIComponent(service.name)}`,
				{ ports, volumes }
			);
			await finishSave(service, result, 'Ports and volumes saved');
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			busy = null;
		}
	}

	async function action(service: ComposeService, kind: 'start' | 'restart') {
		busy = kind;
		try {
			const updated = await post<ComposeService>(
				`/apps/${app.id}/services/${encodeURIComponent(service.name)}/${kind}`
			);
			services = services.map((item) => (item.name === service.name ? { ...item, ...updated } : item));
			notify(`${service.name} ${kind === 'start' ? 'started' : 'restarted'}.`);
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			busy = null;
		}
	}
</script>

{#snippet mappings(list: { value: string }[], placeholder: string, label: string)}
	<div class="grid w-full max-w-xl gap-1.5">
		{#each list as entry, index (index)}
			<div class="flex items-center gap-1.5">
				<input class="input mono h-8" bind:value={entry.value} {placeholder} aria-label={label} spellcheck="false" />
				<button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label="Remove {label}" onclick={() => list.splice(index, 1)}><Icon name="close" size={17} /></button>
			</div>
		{/each}
		<button type="button" class="btn btn-ghost btn-sm justify-self-start" onclick={() => list.push({ value: '' })}><Icon name="add" size={16} /> Add</button>
	</div>
{/snippet}

<section class="panel overflow-hidden">
	<div class="panel-head">
		<h2>
			Services{#if services.length}<span class="ml-1.5 text-muted">{services.length}</span>{/if}
		</h2>
		<a class="btn btn-sm" href="?tab=deployments" data-sveltekit-replacestate><Icon name="description" size={16} /> Edit compose</a>
	</div>

	{#if error && services.length === 0}
		<p class="m-5 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad" role="alert">Could not load services: {error}</p>
	{:else if loading}
		<p class="p-8 text-center text-[13px] text-muted">Loading services…</p>
	{:else if !selected}
		<p class="p-8 text-center text-[13px] text-muted">No services.</p>
	{:else}
		<div class="grid md:grid-cols-[15rem_minmax(0,1fr)]">
			<nav aria-label="Services" class="border-b border-line p-2 md:border-r md:border-b-0">
				<ul class="flex gap-1 overflow-x-auto md:flex-col">
					{#each services as service (service.name)}
						{@const active = service.name === selected.name}
						<li class="shrink-0">
							<button
								type="button"
								class="flex w-full min-w-0 items-center gap-2.5 rounded-md px-3 py-2 text-left {active ? 'bg-indicator text-on-indicator' : 'hover:bg-hover'}"
								aria-current={active ? 'true' : undefined}
								onclick={() => select(service.name)}
							>
								<span class="size-2 shrink-0 rounded-full {dot[status(service)]}" aria-hidden="true" title={status(service).replace('_', ' ')}></span>
								<span class="min-w-0 flex-1">
									<span class="flex items-center gap-1.5">
										<span class="mono truncate text-[13px] font-medium">{service.name}</span>
										{#if service.exposed}<Icon name="open_in_new" size={14} class="opacity-70" label="Public" />{/if}
									</span>
									<span class="mono block max-w-48 truncate text-[11.5px] {active ? 'opacity-80' : 'text-muted'}">{service.image ?? '-'}</span>
								</span>
							</button>
						</li>
					{/each}
				</ul>
			</nav>

			<div class="min-w-0">
				<header class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 pt-4">
					<h3 class="mono text-[17px] font-medium">{selected.name}</h3>
					{#if selected.present}
						<Status status={status(selected)} size="sm" />
					{:else}
						<span class="text-xs font-medium text-muted">Not deployed</span>
					{/if}
					{#if selected.restart_count > 0}
						<span class="text-xs text-muted">{selected.restart_count} {selected.restart_count === 1 ? 'restart' : 'restarts'}</span>
					{/if}
					{#if selected.exposed}
						<span class="rounded-md bg-accent-soft px-1.5 py-0.5 text-[11px] font-medium text-accent-text">Public</span>
					{/if}
					<div class="ml-auto flex flex-wrap gap-2">
						{#if editingConfig || editing}
							<button type="button" class="btn btn-sm" onclick={cancelEdit}>Cancel</button>
							<button class="btn btn-primary btn-sm" form={editingConfig ? 'service-config-form' : 'service-env-form'} disabled={busy !== null}>
								{busy === 'config' || busy === 'environment' ? 'Saving…' : 'Save'}
							</button>
						{:else}
							{#if view === 'overview'}
								<button type="button" class="btn btn-sm" onclick={() => startConfigEdit(selected)}><Icon name="settings" size={16} /> Edit</button>
							{:else if view === 'environment'}
								{#if selected.environment.length}
									<button type="button" class="btn btn-ghost btn-sm" aria-pressed={revealed} onclick={() => (revealed = !revealed)}>
										<Icon name={revealed ? 'visibility_off' : 'visibility'} size={16} /> {revealed ? 'Hide' : 'Show'}
									</button>
								{/if}
								<button type="button" class="btn btn-sm" onclick={() => startEdit(selected)}>
									<Icon name={selected.environment.length ? 'settings' : 'add'} size={16} /> {selected.environment.length ? 'Edit' : 'Add'}
								</button>
							{/if}
							{#if selected.present && selected.running}
								<button type="button" class="btn btn-sm" disabled={busy !== null} onclick={() => action(selected, 'restart')}>
									<Icon name="refresh" size={16} class={busy === 'restart' ? 'animate-spin motion-reduce:animate-none' : ''} /> Restart
								</button>
							{:else if selected.present}
								<button type="button" class="btn btn-primary btn-sm" disabled={busy !== null} onclick={() => action(selected, 'start')}>
									<Icon name="play_arrow" size={16} /> Start
								</button>
							{/if}
						{/if}
					</div>
				</header>

				<div class="mt-2 flex gap-1 border-b border-line px-3" role="tablist">
					{#each views as t (t.id)}
						<button
							type="button"
							role="tab"
							aria-selected={view === t.id}
							class="relative px-3 py-2.5 text-[13px] font-medium hover:text-text {view === t.id ? 'text-accent-text' : 'text-muted'}"
							onclick={() => { cancelEdit(); view = t.id; }}
						>
							{t.label}{#if t.id === 'environment' && selected.environment.length}<span class="ml-1.5 rounded-full bg-raised px-1.5 text-[11px] text-muted">{selected.environment.length}</span>{/if}
							{#if view === t.id}<span class="absolute inset-x-2 bottom-0 h-[2px] rounded-t-[2px] bg-accent-text"></span>{/if}
						</button>
					{/each}
				</div>

				<div class="p-5" role="tabpanel">
					{#if view === 'overview'}
						<form id="service-config-form" onsubmit={(event) => { event.preventDefault(); if (editingConfig) saveServiceConfig(selected); }}>
							<dl class="service-props">
								<dt>Image</dt>
								<dd>
									{#if selected.image}<code class="truncate" title={selected.image}>{selected.image}</code><Copy value={selected.image} label="Copy image" />{:else}<span class="text-muted">-</span>{/if}
								</dd>

								<dt>Container</dt>
								<dd>
									<code class="truncate" title={selected.container_name}>{selected.container_name}</code><Copy value={selected.container_name} label="Copy container name" />
								</dd>

								<dt>Ports</dt>
								<dd>
									{#if editingConfig}
										{@render mappings(configDraft.ports, '8080:80', 'port mapping')}
									{:else}
										{#each selected.ports as port}<code class="rounded-md bg-raised px-2 py-0.5">{port}</code>{:else}<span class="text-muted">-</span>{/each}
									{/if}
								</dd>

								<dt>Volumes</dt>
								<dd>
									{#if editingConfig}
										{@render mappings(configDraft.volumes, 'data:/var/lib/data', 'volume mapping')}
									{:else if selected.volumes.length}
										<ul class="grid min-w-0 gap-1">{#each selected.volumes as volume}<li class="min-w-0"><code class="block truncate" title={volume}>{volume}</code></li>{/each}</ul>
									{:else}<span class="text-muted">-</span>{/if}
								</dd>

								<dt>Depends on</dt>
								<dd>
									{#each selected.depends_on as dependency}
										{#if services.some((s) => s.name === dependency)}
											<button type="button" class="mono rounded-md border border-line px-2 py-0.5 hover:border-accent-text hover:text-accent-text" onclick={() => select(dependency)}>{dependency}</button>
										{:else}
											<code class="rounded-md border border-line px-2 py-0.5">{dependency}</code>
										{/if}
									{:else}<span class="text-muted">-</span>{/each}
								</dd>
							</dl>
						</form>
					{:else if view === 'environment'}
						{#if editing}
							<form id="service-env-form" class="space-y-2" onsubmit={(event) => { event.preventDefault(); saveEnvironment(selected); }}>
								{#each envDraft as entry, index (index)}
									<div class="grid gap-2 sm:grid-cols-[minmax(8rem,0.4fr)_minmax(0,1fr)_auto]">
										<input class="input mono" bind:value={entry.key} placeholder="NAME" aria-label="Variable name" spellcheck="false" />
										<input class="input mono" bind:value={entry.value} placeholder="value" aria-label="Value of {entry.key || 'new variable'}" autocomplete="off" spellcheck="false" />
										<button type="button" class="btn btn-ghost btn-sm btn-icon self-center" aria-label="Remove {entry.key || 'variable'}" onclick={() => envDraft.splice(index, 1)}><Icon name="close" size={17} /></button>
									</div>
								{/each}
								<button type="button" class="btn btn-ghost btn-sm" onclick={() => envDraft.push({ key: '', value: '' })}><Icon name="add" size={16} /> Add variable</button>
							</form>
						{:else}
							{#if selected.environment.length}
								<div class="overflow-hidden rounded-lg border border-line">
									<table class="w-full table-fixed text-[13px]">
										<tbody class="divide-y divide-line">
											{#each selected.environment as entry (entry.key)}
												<tr>
													<th scope="row" class="mono w-2/5 truncate px-3 py-2 text-left font-medium" title={entry.key}>{entry.key}</th>
													<td class="mono truncate px-3 py-2 {revealed ? '' : 'text-muted'}" title={revealed ? (entry.value ?? '') : undefined}>
														{#if entry.value === null}<span class="text-muted">-</span>{:else if revealed}{entry.value}{:else}••••••••{/if}
													</td>
												</tr>
											{/each}
										</tbody>
									</table>
								</div>
							{:else}
								<p class="py-6 text-center text-[13px] text-muted">No environment variables.</p>
							{/if}
						{/if}
					{:else if !selected.present}
						<p class="py-6 text-center text-[13px] text-muted">Not deployed.</p>
					{:else}
						{#key selected.name}
							<LogView path={`/apps/${app.id}/services/${encodeURIComponent(selected.name)}/logs`} height="28rem" reconnect empty="No output yet." />
						{/key}
					{/if}
				</div>
			</div>
		</div>
	{/if}
</section>
