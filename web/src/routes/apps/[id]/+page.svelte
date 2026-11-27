<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { get, post, del, errorMessage, ApiError, type AppDetail } from '#lib/api.ts';
	import { notify } from '#lib/toast.svelte.ts';
	import { internalAddress } from '#lib/types-board.ts';
	import { ago } from '#lib/format.ts';
	import Status from '#lib/components/Status.svelte';
	import Copy from '#lib/components/Copy.svelte';
	import Confirm from '#lib/components/Confirm.svelte';
	import LogView from '#lib/components/LogView.svelte';
	import Deployments from '#lib/components/app/Deployments.svelte';
	import Services from '#lib/components/app/Services.svelte';
	import Domains from '#lib/components/app/Domains.svelte';
	import Config from '#lib/components/app/Config.svelte';
	import Usage from '#lib/components/app/Usage.svelte';
	import AppDetailSkeleton from '#lib/components/AppDetailSkeleton.svelte';

	const id = $derived(page.params.id ?? '');
	let app = $state<AppDetail | null>(null);
	const tabs = $derived([
		{ id: 'deployments', label: 'Deployments' },
		...(app?.deploy_type === 'compose' ? [{ id: 'services', label: 'Services' }] : []),
		{ id: 'domains', label: 'Domains' },
		{ id: 'config', label: 'Config' },
		{ id: 'logs', label: 'Logs' }
	]);
	const tab = $derived(tabs.find((t) => t.id === page.url.searchParams.get('tab'))?.id ?? 'deployments');

	let error = $state('');
	let notFound = $state(false);
	let busy = $state('');
	let configKey = $state(0);

	async function load() {
		try {
			app = await get<AppDetail>(`/apps/${id}`);
			error = '';
		} catch (e) {
			if (e instanceof ApiError && e.status === 404) notFound = true;
			else error = errorMessage(e);
		}
	}

	$effect(() => {
		id;
		app = null;
		notFound = false;
		load();
	});
	// Poll faster while a deployment is in flight.
	$effect(() => {
		const inFlight = app?.deployments.some((d) => d.status === 'queued' || d.status === 'building');
		const timer = setInterval(load, inFlight ? 2500 : 10_000);
		return () => clearInterval(timer);
	});

	async function action(kind: 'deploy' | 'restart' | 'stop' | 'start') {
		busy = kind;
		try {
			await post(`/apps/${id}/${kind}`);
			if (kind === 'deploy') {
				notify('Deployment queued.');
				if (tab !== 'deployments') goto(`?tab=deployments`, { replaceState: true });
			} else notify({ restart: 'Restarted.', stop: 'Stopped.', start: 'Started.' }[kind]);
			await load();
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			busy = '';
		}
	}

	let deleteOpen = $state(false);
	let deleting = $state(false);
	let removeVolumes = $state(false);
	async function remove() {
		if (!app) return;
		deleting = true;
		try {
			await del(`/apps/${app.id}?remove_volumes=${removeVolumes}`);
			notify(`Deleted ${app.name}.`);
			goto('/');
		} catch (e) {
			notify(errorMessage(e), 'bad');
			deleting = false;
		}
	}

	const typeIcon = { draft: 'settings', image: 'deployed_code', git: 'commit', compose: 'stacks' } as const;
	const deployed = $derived(!!app?.active_deployment_id);
	const inFlight = $derived(!!app?.deployments.some((d) => d.status === 'queued' || d.status === 'building'));
	const internal = $derived(app ? internalAddress(app) : null);
	const latest = $derived(app?.deployments[0]);
	const lastFailed = $derived(!!latest && latest.status === 'failed' && !!app?.active_deployment_id);
</script>

<a href="/" class="inline-flex items-center gap-1.5 text-[13px] text-muted hover:text-text"><Icon name="arrow_back" size={17} /> Apps</a>

{#if notFound}
	<div class="panel mt-4 p-10 text-center">
		<h1 class="text-[15px] font-semibold">App not found</h1>
	</div>
{:else if !app}
	{#if error}
		<p class="mt-6 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad" role="alert">Could not load the app: {error}</p>
	{:else}
		<AppDetailSkeleton />
	{/if}
{:else}
	{@const typeIconName = typeIcon[app.deploy_type]}
	<header class="mt-2 flex flex-wrap items-start justify-between gap-4">
		<div class="min-w-0">
			<div class="flex flex-wrap items-center gap-3">
				<h1 class="page-title">{app.name}</h1>
				<Status status={app.status} />
			</div>
			<div class="mt-1.5 flex flex-wrap items-center gap-x-4 gap-y-1 text-[13px] text-muted">
				<span class="flex items-center gap-1.5"><Icon name={typeIconName} size={17} />
					{#if app.deploy_type === 'draft'}Not configured
					{:else if app.deploy_type === 'image'}<span class="mono">{app.image_name}</span>
					{:else if app.deploy_type === 'git'}<span class="mono">{app.git_repo_url}</span> · {app.git_branch}
					{:else}Compose stack{app.compose_service ? ` · exposes ${app.compose_service}` : ' · private'}{/if}
				</span>
				{#if internal}
					<span class="flex items-center" title="Internal address"><Icon name="cable" size={16} class="mr-1.5" /><code class="text-text">{internal}</code><Copy value={internal} label="Copy internal address" /></span>
				{/if}
				<Usage {app} />
			</div>
		</div>
		<div class="flex flex-wrap gap-2">
			{#if deployed}
				{#if app.status === 'stopped'}
					<button class="btn" disabled={!!busy || inFlight} onclick={() => action('start')}><Icon name="play_arrow" size={18} /> Start</button>
				{:else}
					<button class="btn" disabled={!!busy || inFlight} onclick={() => action('restart')}><Icon name="refresh" size={18} /> Restart</button>
					<button class="btn" disabled={!!busy || inFlight} onclick={() => action('stop')}><Icon name="stop" size={17} /> Stop</button>
				{/if}
			{/if}
			{#if app.deploy_type !== 'draft'}
				<button class="btn btn-primary" disabled={!!busy} onclick={() => action('deploy')}>
					<Icon name="rocket_launch" size={18} /> {busy === 'deploy' ? 'Queuing…' : inFlight ? 'Deploy again' : 'Deploy'}
				</button>
			{/if}
			<button class="btn btn-icon text-bad" aria-label="Delete {app.name}" title="Delete app" onclick={() => (deleteOpen = true)}><Icon name="delete" size={18} /></button>
		</div>
	</header>

	{#if lastFailed && latest}
		<p class="mt-4 flex flex-wrap items-center gap-x-2 rounded-lg border border-warn/40 bg-warn-soft px-4 py-2.5 text-[13px]">
			<span class="font-medium text-warn">Last deploy failed {ago(latest.started_at)}.</span>
			<span class="text-muted">Previous version still live.</span>
			{#if tab !== 'deployments'}<a class="link" href="?tab=deployments" data-sveltekit-replacestate>See the build log</a>{/if}
		</p>
	{/if}

	<nav class="mt-6 flex overflow-x-auto overflow-y-hidden border-b border-line" aria-label="App sections">
		{#each tabs as t (t.id)}
			<a
				href="?tab={t.id}"
				aria-current={tab === t.id ? 'page' : undefined}
				data-sveltekit-replacestate
				class="relative px-3.5 py-2.5 text-[13.5px] font-medium whitespace-nowrap hover:bg-hover {tab === t.id
					? 'text-accent-text'
					: 'text-muted hover:text-text'}"
				>{t.label}{#if tab === t.id}<span class="absolute inset-x-2 bottom-0 h-[2px] rounded-t-[2px] bg-accent-text"></span>{/if}</a
			>
		{/each}
	</nav>

	<div class="mt-6">
		{#if tab === 'deployments'}
			<Deployments {app} onchange={load} />
		{:else if tab === 'services' && app.deploy_type === 'compose'}
				<Services {app} onchange={load} />
		{:else if tab === 'domains'}
			{#key `${app.exposed}|${app.default_domain}|` + app.domains.map((d) => d.hostname + d.force_https).join()}
				<Domains {app} onchange={load} />
			{/key}
		{:else if tab === 'config'}
			{#key configKey}
					<Config {app} onsaved={async () => { await load(); configKey++; }} />
			{/key}
		{:else if tab === 'logs'}
			{#if deployed && app.active_container_id}
				<!-- A redeploy swaps the container: follow the new one. -->
				{#key app.active_container_id}
					<LogView path={`/apps/${app.id}/logs`} fill reconnect empty="No output yet." />
				{/key}
			{:else}
				<p class="panel p-8 text-center text-muted">{app.deploy_type === 'compose' && !app.compose_service ? 'No exposed service.' : 'Not deployed yet.'}</p>
			{/if}
		{/if}
	</div>
{/if}

<Confirm bind:open={deleteOpen} title="Delete {app?.name ?? 'app'}?" confirmLabel="Delete app" danger busy={deleting} onconfirm={remove}>
	Stops its containers and removes its routes, config and history. This cannot be undone.
	<label class="mt-3 flex items-center gap-2 text-text">
		<input type="checkbox" bind:checked={removeVolumes} /> Also delete its data volumes
	</label>
</Confirm>
