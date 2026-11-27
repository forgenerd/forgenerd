<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import { get, post, errorMessage, type Blueprint, type BlueprintDeployed, type BlueprintList, type Project } from '#lib/api.ts';
	import { ago } from '#lib/format.ts';
	import Skeleton from '#lib/components/Skeleton.svelte';
	import './blueprints.css';

	let blueprints = $state<Blueprint[]>([]);
	let projects = $state<Project[]>([]);
	let selected = $state<Blueprint | null>(null);
	let dialog: HTMLDialogElement | undefined = $state();
	let query = $state('');
	let name = $state('');
	let projectId = $state('');
	let values = $state<Record<string, string>>({});
	let imageTag = $state('');
	let busy = $state(false);
	let loaded = $state(false);
	let error = $state('');
	let catalogUpdated = $state<string | null>(null);
	let catalogError = $state<string | null>(null);
	let refreshing = $state(false);

	function applyCatalog(list: BlueprintList) {
		blueprints = list.blueprints;
		catalogUpdated = list.updated_at;
		catalogError = list.error;
	}

	async function refreshCatalog() {
		refreshing = true;
		try {
			applyCatalog(await post<BlueprintList>('/blueprints/refresh'));
		} catch (e) {
			catalogError = errorMessage(e);
		} finally {
			refreshing = false;
		}
	}
	$effect(() => {
		if (!dialog) return;
		if (selected && !dialog.open) dialog.showModal();
		if (!selected && dialog.open) dialog.close();
	});

	onMount(async () => {
		try {
			const [list, nextProjects] = await Promise.all([get<BlueprintList>('/blueprints'), get<Project[]>('/projects')]);
			applyCatalog(list);
			projects = nextProjects;
			projectId = projects[0]?.id ?? '';
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loaded = true;
		}
	});

	const visible = $derived(
		blueprints.filter((blueprint) => {
			const needle = query.trim().toLowerCase();
			return !needle || `${blueprint.name} ${blueprint.category} ${blueprint.description}`.toLowerCase().includes(needle);
		})
	);

	// Categories in the order the server lists them (databases first).
	const groups = $derived(
		visible.reduce<{ category: string; items: Blueprint[] }[]>((acc, blueprint) => {
			const group = acc.find((g) => g.category === blueprint.category);
			if (group) group.items.push(blueprint);
			else acc.push({ category: blueprint.category, items: [blueprint] });
			return acc;
		}, [])
	);


	function initials(name: string) {
		return name.replace(/[^A-Za-z0-9]/g, '').slice(0, 2);
	}

	function parameterLabel(key: string) {
		return key
			.toLowerCase()
			.split('_')
			.map((word) => (word === 'mysql' ? 'MySQL' : word === 'postgres' ? 'PostgreSQL' : word === 'redis' ? 'Redis' : word.charAt(0).toUpperCase() + word.slice(1)))
			.join(' ');
	}

	function start(blueprint: Blueprint) {
		selected = blueprint;
		name = blueprint.id === 'postgres' ? 'db' : blueprint.id;
		values = Object.fromEntries(blueprint.parameters.map((parameter) => [parameter.key, parameter.default_value ?? '']));
		imageTag = blueprint.image_tag;
		error = '';
	}

	function cancel() {
		selected = null;
		error = '';
	}

	async function deploy(e: SubmitEvent) {
		e.preventDefault();
		if (!selected) return;
		busy = true;
		error = '';
		try {
			let pid = projectId;
			if (!pid) pid = (await post<Project>('/projects', { name: 'default' })).id;
			const blueprint = selected;
			const r = await post<BlueprintDeployed>(`/blueprints/${blueprint.id}/deploy`, {
				project_id: pid,
				name: name.trim(),
				image_tag: imageTag.trim(),
				env: values
			});
			selected = null;
			await goto(`/apps/${r.app.id}`, { state: { blueprintDeployment: r } });
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

{#snippet logo(blueprint: Blueprint, cls: string)}
	{@const src = blueprint.logo}
	{#if src}
		<span class="blueprint-logo {cls}"><img {src} alt="" /></span>
	{:else}
		<span class="blueprint-logo is-initials {cls}" aria-hidden="true">{initials(blueprint.name)}</span>
	{/if}
{/snippet}


	<dialog
		bind:this={dialog}
		onclose={() => (selected = null)}
		aria-labelledby="blueprint-dialog-title"
		class="blueprint-dialog"
	>
		{#if selected}
			<header class="flex items-start justify-between gap-4 border-b border-line px-6 py-5">
				<div class="flex min-w-0 items-center gap-4">
					{@render logo(selected, 'size-12 rounded-lg')}
					<div class="min-w-0">
						<h2 id="blueprint-dialog-title" class="truncate text-[18px] font-semibold">{selected.name}</h2>
					</div>
				</div>
				<button type="button" class="btn btn-ghost btn-sm btn-icon shrink-0" onclick={cancel} aria-label="Close blueprint configuration"><Icon name="close" size={19} /></button>
			</header>

			<form class="blueprint-dialog-form" onsubmit={deploy}>
				<div class="blueprint-dialog-body p-6">
					<section>
						<h3 class="text-[14px] font-semibold">Deployment</h3>
						<div class="mt-4 grid gap-5 sm:grid-cols-2">
							<label>
								<span class="label">App name</span>
								<input class="input" bind:value={name} required pattern="[a-z0-9]([a-z0-9-]*[a-z0-9])?" autocomplete="off" />
								{#if selected.expose}
									<span class="mt-1 block text-xs text-muted">Public web app, served at <code>{name || 'name'}.&lt;root domain&gt;</code></span>
								{:else}
									<code class="mt-1 block text-xs text-muted" title="Internal address">{name || 'name'}:{selected.port}</code>
								{/if}
							</label>

							{#if projects.length > 0}
								<label>
									<span class="label">Project</span>
									<select class="input" bind:value={projectId}>
										{#each projects as project (project.id)}<option value={project.id}>{project.name}</option>{/each}
									</select>
								</label>
							{/if}

							<label class="sm:col-span-2">
								<span class="label">Image tag <code class="font-normal">{selected.image}</code></span>
								<input class="input mono" bind:value={imageTag} required maxlength="128" pattern="[A-Za-z0-9][A-Za-z0-9_.-]*" autocomplete="off" spellcheck="false" />
							</label>
						</div>
					</section>

					<section class="mt-6 border-t border-line pt-5">
						<h3 class="text-[14px] font-semibold">Configuration</h3>
						{#if selected.parameters.length === 0}<p class="mt-2 text-[13px] text-muted">Nothing to configure.</p>{/if}
						<div class="mt-4 grid gap-5 sm:grid-cols-2">
							{#each selected.parameters as parameter (parameter.key)}
								<label>
									<span class="label">{parameterLabel(parameter.key)}</span>
									<input
										class="input"
										type={parameter.secret ? 'password' : 'text'}
										bind:value={values[parameter.key]}
										required={parameter.required}
										autocomplete={parameter.secret ? 'new-password' : 'off'}
										placeholder={parameter.generate ? 'Auto-generated' : parameter.public_url ? "The app's URL" : undefined}
									/>
									{#if parameter.description}<span class="mt-1 block text-xs text-muted">{parameter.description}</span>{/if}
								</label>
							{/each}
						</div>
					</section>

					{#if error}<p role="alert" class="mt-5 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">{error}</p>{/if}
					</div>
				<footer class="blueprint-dialog-footer flex flex-wrap justify-end gap-2 border-t border-line px-6 py-4">
					<button type="button" class="btn" onclick={cancel}>Cancel</button>
					<button class="btn btn-primary" disabled={busy}><Icon name={busy ? 'progress_activity' : 'rocket_launch'} size={18} class={busy ? 'animate-spin motion-reduce:animate-none' : ''} /> {busy ? 'Deploying…' : 'Deploy'}</button>
				</footer>
			</form>
		{/if}
	</dialog>

	<h1 class="page-title">Blueprints</h1>

	<div class="mt-6 flex flex-wrap items-center justify-between gap-3">
		<label class="relative w-full sm:max-w-sm">
			<Icon name="search" size={18} class="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-muted" />
			<input class="input pl-8" placeholder="Search blueprints" bind:value={query} aria-label="Search blueprints" />
		</label>
		<div class="flex items-center gap-3 text-[13px] text-muted">
			{#if catalogUpdated}<span title="Blueprints come from the forgenerd/blueprints catalog">Catalog updated {ago(catalogUpdated)}</span>{/if}
			<button class="btn btn-sm" onclick={refreshCatalog} disabled={refreshing}>
				<Icon name={refreshing ? 'progress_activity' : 'refresh'} size={17} class={refreshing ? 'animate-spin motion-reduce:animate-none' : ''} />
				{refreshing ? 'Refreshing…' : 'Refresh'}
			</button>
		</div>
	</div>

	{#if catalogError}
		<p role="alert" class="mt-4 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">
			Could not update the blueprint catalog: {catalogError}{blueprints.length ? ' Showing the last catalog downloaded.' : ''}
		</p>
	{/if}

	{#if error}
		<p role="alert" class="mt-5 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">{error}</p>
	{:else if !loaded}
		<ul class="panel mt-5 divide-y divide-line overflow-hidden" aria-busy="true" aria-label="Loading blueprints">
			{#each Array(3) as _, i (i)}
				<li>
					<div class="blueprint-row">
						<span class="blueprint-row-logo"><Skeleton class="size-10" /></span>
						<span class="blueprint-row-content grid gap-2">
							<Skeleton class="h-4 w-32" />
							<Skeleton class="h-3 w-3/4 max-w-[32rem]" />
						</span>
						<span class="blueprint-row-side"><Skeleton class="h-3 w-16" /><Skeleton class="h-3 w-20" /></span>
					</div>
				</li>
			{/each}
		</ul>
	{:else}
		{#each groups as group (group.category)}
			<section class="mt-5" aria-labelledby="blueprints-{group.category}">
				<h2 id="blueprints-{group.category}" class="mb-2 text-[13px] font-semibold text-muted">{group.category} <span class="font-normal">({group.items.length})</span></h2>
				<ul class="panel divide-y divide-line overflow-hidden">
					{#each group.items as blueprint (blueprint.id)}
						<li>
							<button type="button" class="group blueprint-row" onclick={() => start(blueprint)} aria-label="Configure {blueprint.name}">
								{@render logo(blueprint, 'blueprint-row-logo')}
								<span class="blueprint-row-content">
									<span class="blueprint-row-heading">
										<span class="blueprint-row-title">{blueprint.name}</span>
										<Icon name="chevron_right" size={19} class="shrink-0 text-muted transition-transform group-hover:translate-x-0.5" />
									</span>
									<span class="blueprint-row-description">{blueprint.description}</span>
								</span>
								<span class="blueprint-row-side">
									<span class="blueprint-row-port mono">{blueprint.expose ? 'public' : `${blueprint.port}/tcp`}</span>
									<span class="blueprint-row-action">Configure</span>
								</span>
							</button>
						</li>
					{/each}
				</ul>
			</section>
		{:else}
			<p class="panel mt-5 px-5 py-10 text-center text-muted">
				{#if blueprints.length === 0}
					No blueprints yet. They are downloaded from the blueprint catalog; use Refresh to try again.
				{:else}
					No blueprints match “{query}”.
				{/if}
			</p>
		{/each}
	{/if}
