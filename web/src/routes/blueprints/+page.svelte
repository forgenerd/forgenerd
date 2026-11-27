<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import { get, post, errorMessage, type Blueprint, type Project } from '#lib/api.ts';
	import Copy from '#lib/components/Copy.svelte';
	import Skeleton from '#lib/components/Skeleton.svelte';
	import './blueprints.css';
	import mysqlLogo from '../../../../blueprints/logos/mysql.png';
	import postgresLogo from '../../../../blueprints/logos/postgres.png';
	import redisLogo from '../../../../blueprints/logos/redis.png';

	const blueprintLogos: Record<string, string> = {
		mysql: mysqlLogo,
		postgres: postgresLogo,
		redis: redisLogo
	};

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
	let result = $state<{ app: { id: string; name: string }; connection_string: string | null; internal_host: string; blueprint: Blueprint } | null>(null);

	$effect(() => {
		if (!dialog) return;
		if (selected && !dialog.open) dialog.showModal();
		if (!selected && dialog.open) dialog.close();
	});

	onMount(async () => {
		try {
			[blueprints, projects] = await Promise.all([get<Blueprint[]>('/blueprints'), get<Project[]>('/projects')]);
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
			return !needle || `${blueprint.name} ${blueprint.description}`.toLowerCase().includes(needle);
		})
	);

	function logoFor(id: string) {
		return blueprintLogos[id];
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
			const r = await post<{ app: { id: string; name: string }; connection_string: string | null; internal_host: string }>(
				`/blueprints/${blueprint.id}/deploy`,
				{ project_id: pid, name: name.trim(), image_tag: imageTag.trim(), env: values }
			);
			result = { ...r, blueprint };
			selected = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>


	<dialog
		bind:this={dialog}
		onclose={() => (selected = null)}
		aria-labelledby="blueprint-dialog-title"
		class="blueprint-dialog"
	>
		{#if selected}
			<header class="flex items-start justify-between gap-4 border-b border-line px-6 py-5">
				<div class="flex min-w-0 items-center gap-4">
					<div class="grid size-12 shrink-0 place-items-center rounded-lg bg-white p-2">
						<img src={logoFor(selected.id)} alt="" class="max-h-full max-w-full object-contain" />
					</div>
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
								<code class="mt-1 block text-xs text-muted" title="Internal address">{name || 'name'}:{selected.port}</code>
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
						<div class="mt-4 grid gap-5 sm:grid-cols-2">
							{#each selected.parameters as parameter (parameter.key)}
								<label>
									<span class="label">{parameterLabel(parameter.key)}</span>
									<input
										class="input"
										type={parameter.secret ? 'password' : 'text'}
										bind:value={values[parameter.key]}
										required={!parameter.generate}
										autocomplete={parameter.secret ? 'new-password' : 'off'}
										placeholder={parameter.generate ? 'Auto-generated' : undefined}
										title={parameter.description || undefined}
									/>
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
	</div>

	{#if result}
		<section class="mt-5 rounded-lg border border-warn/40 bg-warn-soft p-5" aria-live="polite">
			<h2 class="flex items-center gap-2 text-[14px] font-semibold"><Icon name="warning" size={19} class="text-warn" /> Copy the connection string now</h2>
			<p class="mt-1 text-[13px] text-muted">
				Shown once. Deploying as <a class="link" href="/apps/{result.app.id}">{result.app.name}</a>.
			</p>
			{#if result.connection_string}
				<div class="mt-3 flex items-center gap-1">
					<code class="input flex items-center overflow-x-auto bg-surface whitespace-nowrap">{result.connection_string}</code>
					<Copy value={result.connection_string} label="Copy connection string" />
				</div>
			{/if}
			<button class="btn btn-sm mt-3" onclick={() => (result = null)}>I've saved it</button>
		</section>
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
		<ul class="panel mt-5 divide-y divide-line overflow-hidden">
			{#each visible as blueprint (blueprint.id)}
				<li>
					<button type="button" class="group blueprint-row" onclick={() => start(blueprint)} aria-label="Configure {blueprint.name}">
						<span class="blueprint-row-logo">
							<img src={logoFor(blueprint.id)} alt="" />
						</span>
						<span class="blueprint-row-content">
							<span class="blueprint-row-heading">
								<span class="blueprint-row-title">{blueprint.name}</span>
								<Icon name="chevron_right" size={19} class="shrink-0 text-muted transition-transform group-hover:translate-x-0.5" />
							</span>
							<span class="blueprint-row-description">{blueprint.description}</span>
						</span>
						<span class="blueprint-row-side">
							<span class="blueprint-row-port mono">{blueprint.port}/tcp</span>
							<span class="blueprint-row-action">Configure</span>
						</span>
					</button>
				</li>
			{:else}
				<li class="px-5 py-10 text-center text-muted">No blueprints match “{query}”.</li>
			{/each}
		</ul>
	{/if}
