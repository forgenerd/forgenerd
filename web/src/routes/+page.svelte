<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { get, post, del, errorMessage, type AppDetail, type Project, type Settings } from '#lib/api.ts';
	import { ago, shortSha } from '#lib/format.ts';
	import { sortBoard, needsAttention, deployFailed, internalAddress, type AppSummary } from '#lib/types-board.ts';
	import Status from '#lib/components/Status.svelte';
	import Confirm from '#lib/components/Confirm.svelte';
	import Skeleton from '#lib/components/Skeleton.svelte';
	import { notify } from '#lib/toast.svelte.ts';
	import './apps.css';

	let apps = $state<AppSummary[]>([]);
	let projects = $state<Project[]>([]);
	let rootDomain = $state('');
	let loaded = $state(false);
	let error = $state('');
	let query = $state('');
	let project = $state('');
	let now = $state(Date.now());
	let deleteOpen = $state(false);
	let deleteTarget = $state<AppSummary | null>(null);
	let deleting = $state(false);
	let removeVolumes = $state(false);
	let createOpen = $state(false);
	let createDialog: HTMLDialogElement | undefined = $state();
	let createName = $state('');
	let createProjectId = $state('');
	let createPersistent = $state(false);
	let creating = $state(false);
	let createError = $state('');

	$effect(() => {
		if (!createDialog) return;
		if (createOpen && !createDialog.open) createDialog.showModal();
		if (!createOpen && createDialog.open) createDialog.close();
	});

	async function refresh() {
		try {
			[apps, projects] = await Promise.all([get<AppSummary[]>('/apps'), get<Project[]>('/projects')]);
			error = '';
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loaded = true;
			now = Date.now();
		}
	}

	function askDelete(app: AppSummary) {
		deleteTarget = app;
		removeVolumes = false;
		deleteOpen = true;
	}

	function openCreate() {
		createName = '';
		createProjectId = projects[0]?.id ?? '';
		createPersistent = false;
		createError = '';
		createOpen = true;
	}

	function closeCreate() {
		if (!creating) createOpen = false;
	}

	async function createApp(e: SubmitEvent) {
		e.preventDefault();
		creating = true;
		createError = '';
		try {
			let pid = createProjectId;
			if (!pid) pid = (await post<Project>('/projects', { name: 'default' })).id;
			const app = await post<AppDetail>('/apps', {
				project_id: pid,
				name: createName.trim(),
				deploy_type: 'image',
				image_name: 'ghcr.io/forgenerd/default-site:latest',
				volumes: createPersistent ? [{ name: 'data', mount_path: '/data' }] : [],
				container_port: 80,
				healthcheck_path: '/',
				exposed: true
			});
			try {
				await post(`/apps/${app.id}/deploy`);
			} catch (e) {
				notify(`App created, but the default deployment was not queued: ${errorMessage(e)}`, 'bad');
			}
			createOpen = false;
			await goto(`/apps/${app.id}?tab=deployments`);
		} catch (e) {
			createError = errorMessage(e);
		} finally {
			creating = false;
		}
	}

	async function remove() {
		const target = deleteTarget;
		if (!target) return;
		deleting = true;
		try {
			await del(`/apps/${target.id}?remove_volumes=${removeVolumes}`);
			notify(`Deleted ${target.name}.`);
			deleteOpen = false;
			deleteTarget = null;
			removeVolumes = false;
			await refresh();
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			deleting = false;
		}
	}

	onMount(() => {
		refresh();
		get<Settings>('/settings').then((settings) => (rootDomain = settings.root_domain ?? '')).catch(() => {});
		const timer = setInterval(refresh, 10_000);
		return () => clearInterval(timer);
	});

	const projectName = (id: string) => projects.find((p) => p.id === id)?.name ?? '';
	const shown = $derived(
		sortBoard(apps).filter(
			(a) =>
				(!project || a.project_id === project) &&
				(!query || a.name.includes(query.toLowerCase()) || a.domains.some((d) => d.includes(query.toLowerCase())))
		)
	);
	const typeIcon = { draft: 'settings', image: 'deployed_code', git: 'commit', compose: 'stacks' } as const;
	const typeLabel = { draft: 'Not configured', image: 'Image', git: 'Git', compose: 'Compose' };

	function source(a: AppSummary): string {
		if (a.deploy_type === 'image') return a.image_name ?? '';
		if (a.deploy_type === 'git') return `${(a.git_repo_url ?? '').replace(/^https:\/\/|\.git$/g, '')} · ${a.git_branch}`;
		if (a.deploy_type === 'draft') return 'Choose a deployment method';
		return a.compose_service ? `exposes ${a.compose_service}` : 'private stack';
	}
	const rowTone = (a: AppSummary) => (needsAttention(a) ? 'bg-bad-soft' : deployFailed(a) ? 'bg-warn-soft' : '');

	function openApp(a: AppSummary) {
		goto(`/apps/${a.id}`);
	}

	function openAppFromRow(event: MouseEvent | KeyboardEvent, a: AppSummary) {
		const target = event.target;
		if (target instanceof Element && target.closest('a,button')) return;
		if (event instanceof KeyboardEvent) {
			if (event.key !== 'Enter' && event.key !== ' ') return;
			event.preventDefault();
		}
		openApp(a);
	}
</script>

{#snippet lastDeploy(a: AppSummary)}
	{#if a.last_deployment}
		{@const d = a.last_deployment}
		<div class="flex items-center gap-2">
			<Status status={d.status} size="sm" />
			<span class="text-muted">{ago(d.started_at, now)}</span>
		</div>
		{#if deployFailed(a)}
			<div class="mt-0.5 text-xs text-muted">Previous version still live</div>
		{:else if d.commit_sha}<div class="mono mt-0.5 text-xs text-muted">{shortSha(d.commit_sha)}</div>{/if}
	{:else}
		<span class="text-muted">Never deployed</span>
	{/if}
{/snippet}

{#snippet addresses(a: AppSummary)}
	{@const internal = internalAddress(a)}
	{#if a.domains.length > 0}
		<a class="hover:underline" href="https://{a.domains[0]}" target="_blank" rel="noreferrer">{a.domains[0]}</a>
		{#if a.domains.length > 1}<span class="text-muted"> +{a.domains.length - 1}</span>{/if}
	{:else}
		<span class="text-muted">No domain</span>
	{/if}
	{#if internal}<div class="mono mt-0.5 text-xs text-muted" title="Internal address for other apps">{internal}</div>{/if}
{/snippet}

<div class="flex flex-wrap items-center justify-between gap-4">
	<h1 class="page-title flex items-center gap-2.5">
		Apps
		{#if loaded && apps.length > 0}<span class="rounded-full bg-raised px-2 py-0.5 text-xs font-medium text-muted">{apps.length}</span>{/if}
	</h1>
	<button class="btn btn-primary" onclick={openCreate}><Icon name="add" size={19} /> New app</button>
</div>

{#if error}
	<p role="alert" class="mt-4 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">Could not load apps: {error}</p>
{/if}

{#if !loaded && !error}
	<div class="mt-6" aria-busy="true" aria-label="Loading apps">
		<div class="panel overflow-hidden divide-y divide-line">
			{#each Array(5) as _, i (i)}
				<div class="grid items-center gap-4 px-4 py-4 sm:grid-cols-[minmax(15rem,1.2fr)_8rem_minmax(13rem,1fr)_9rem_2rem] sm:px-5">
					<div class="grid gap-2">
						<Skeleton class="h-4 w-28" />
						<Skeleton class="h-3 w-48 max-w-full" />
					</div>
					<Skeleton class="h-4 w-20" />
					<div class="grid gap-2">
						<Skeleton class="h-3 w-32" />
						<Skeleton class="h-3 w-24" />
					</div>
					<Skeleton class="h-4 w-24" />
					<Skeleton class="ml-auto h-8 w-8" />
				</div>
			{/each}
		</div>
	</div>
{:else if loaded && apps.length === 0 && !error}
	<div class="panel mt-6 px-6 py-12 text-center">
		<Icon name="deployed_code" size={32} class="mx-auto text-muted" />
		<h2 class="mt-3 text-[15px] font-semibold">No apps yet</h2>
		<div class="mt-5 flex justify-center gap-2">
			<button class="btn btn-primary" onclick={openCreate}><Icon name="add" size={19} /> New app</button>
			<a href="/blueprints" class="btn"><Icon name="dashboard_customize" size={18} /> From a blueprint</a>
		</div>
	</div>
{:else if apps.length > 0}
	<div class="mt-6 flex flex-wrap items-center gap-2">
		<label class="relative w-full max-w-xs">
			<Icon name="search" size={18} class="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-muted" />
			<input class="input pl-8" placeholder="Search apps or domains" bind:value={query} aria-label="Search apps" />
		</label>
		{#if projects.length > 1}
			<select class="input w-auto" bind:value={project} aria-label="Filter by project">
				<option value="">All projects</option>
				{#each projects as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
			</select>
		{/if}
	</div>

	<!-- Narrow screens: one stacked row per app so status, last deploy and address stay visible. -->
	<ul class="panel mt-3 divide-y divide-line sm:hidden">
		{#each shown as a (a.id)}
			<li class={rowTone(a)}>
				<div class="flex items-start gap-3 px-4 py-3">
					<a href="/apps/{a.id}" class="flex min-w-0 flex-1 items-start gap-3">
						<div class="min-w-0 flex-1 text-[13px]">
							<div class="flex items-center justify-between gap-2">
								<span class="text-[15px] font-medium text-accent-text">{a.name}</span>
								<Status status={a.status} size="sm" />
							</div>
							<div class="mt-1">{@render lastDeploy(a)}</div>
							<div class="mt-1 truncate text-muted">
								{a.domains[0] ?? 'No domain'}{#if internalAddress(a)}<span class="mono text-xs">{' · '}{internalAddress(a)}</span>{/if}
							</div>
						</div>
						<Icon name="chevron_right" size={19} class="mt-1 shrink-0 text-muted" />
					</a>
					<button class="btn btn-ghost btn-sm btn-icon text-bad" aria-label="Delete {a.name}" onclick={() => askDelete(a)}>
						<Icon name="delete" size={18} />
					</button>
				</div>
			</li>
		{:else}
			<li class="px-4 py-8 text-center text-muted">No apps match “{query}”.</li>
		{/each}
	</ul>

	<div class="panel mt-3 hidden overflow-x-auto sm:block">
		<table class="table min-w-[760px]">
			<thead>
				<tr>
					<th>App</th>
					<th>Status</th>
					<th>Address</th>
					<th>Last deploy</th>
					<th class="w-0"><span class="sr-only">Open</span></th>
				</tr>
			</thead>
			<tbody>
				{#each shown as a (a.id)}
					{@const typeIconName = typeIcon[a.deploy_type]}
					<tr
						class="cursor-pointer {rowTone(a)}"
						role="link"
						tabindex="0"
						aria-label="Open {a.name}"
						onclick={(event) => openAppFromRow(event, a)}
						onkeydown={(event) => openAppFromRow(event, a)}
					>
						<td>
							<a href="/apps/{a.id}" class="link text-[14px]">{a.name}</a>
							<div class="mt-0.5 flex items-center gap-1.5 text-xs text-muted">
								<Icon name={typeIconName} size={16} />
								<span>{typeLabel[a.deploy_type]}</span>
								<span class="max-w-[260px] truncate {a.deploy_type === 'compose' ? '' : 'mono'}" title={source(a)}>· {source(a)}</span>
								{#if projects.length > 1}<span>· {projectName(a.project_id)}</span>{/if}
							</div>
						</td>
						<td><Status status={a.status} /></td>
						<td class="text-[13px]">{@render addresses(a)}</td>
						<td class="text-[13px]">{@render lastDeploy(a)}</td>
						<td class="text-right">
							<div class="flex justify-end">
								{#if a.domains.length > 0 && a.status === 'running'}
									<a href="https://{a.domains[0]}" target="_blank" rel="noreferrer" class="btn btn-ghost btn-sm btn-icon" aria-label="Open {a.domains[0]}">
										<Icon name="open_in_new" size={18} />
									</a>
								{/if}
								<button class="btn btn-ghost btn-sm btn-icon text-bad" aria-label="Delete {a.name}" onclick={() => askDelete(a)}>
									<Icon name="delete" size={18} />
								</button>
							</div>
						</td>
					</tr>
				{:else}
					<tr><td colspan="5" class="py-10 text-center text-muted">No apps match “{query}”.</td></tr>
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<dialog
	bind:this={createDialog}
	class="app-create-dialog"
	aria-labelledby="create-app-title"
	onclose={() => (createOpen = false)}
	oncancel={(e) => { if (creating) e.preventDefault(); }}
>
	<header class="app-create-dialog-header">
		<Icon name="add" size={20} />
		<h2 id="create-app-title">New app</h2>
		<button type="button" class="btn btn-ghost btn-sm btn-icon" onclick={closeCreate} aria-label="Close create app dialog">
			<Icon name="close" size={19} />
		</button>
	</header>
	<form class="app-create-dialog-body" onsubmit={createApp}>
		<div class="app-create-name-row">
			<label class="sr-only" for="create-app-name">App name</label>
			<input id="create-app-name" class="input" bind:value={createName} required pattern="[a-z0-9]([a-z0-9-]*[a-z0-9])?" maxlength="63" placeholder="my-app" autocomplete="off" />
			<button class="btn btn-primary" disabled={creating}>{creating ? 'Creating…' : 'Create'}</button>
		</div>

		<label class="app-create-field">
			<span class="label">Project</span>
			<select class="input" bind:value={createProjectId}>
				{#each projects as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
				{#if projects.length === 0}<option value="">Create default project</option>{/if}
			</select>
		</label>

		<label class="app-create-checkbox">
			<input type="checkbox" class="size-4" bind:checked={createPersistent} />
			<span class="text-[13px]">Persistent volume at <code>/data</code></span>
		</label>

		<div class="app-create-divider"><span>or</span></div>
		<a class="app-create-blueprints btn" href="/blueprints" onclick={closeCreate}>
			<Icon name="dashboard_customize" size={18} /> From a blueprint
		</a>

		{#if createError}<p role="alert" class="rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">{createError}</p>{/if}
	</form>
</dialog>

<Confirm bind:open={deleteOpen} title="Delete {deleteTarget?.name ?? 'app'}?" confirmLabel="Delete app" danger busy={deleting} onconfirm={remove}>
	This cannot be undone.
	<label class="mt-3 flex items-center gap-2 text-text">
		<input type="checkbox" bind:checked={removeVolumes} /> Also delete its data volumes
	</label>
</Confirm>
