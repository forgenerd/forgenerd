<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { get, post, errorMessage, type AppDetail, type Deployment, type DeployType, type Settings } from '#lib/api.ts';
	import { ago, dateTime, duration, shortId, shortSha } from '#lib/format.ts';
	import { notify } from '#lib/toast.svelte.ts';
	import Status from '#lib/components/Status.svelte';
	import LogView from '#lib/components/LogView.svelte';
	import Copy from '#lib/components/Copy.svelte';
	import Confirm from '#lib/components/Confirm.svelte';

	let { app, onchange }: { app: AppDetail; onchange: () => void } = $props();

	let selected = $state<string | null>(null);
	let logsOpen = $state(false);
	let keepLogsOpen = $state(false);
	const inFlightDeployment = $derived(app.deployments.find((d) => d.status === 'queued' || d.status === 'building'));
	const current = $derived(inFlightDeployment?.id ?? selected ?? app.deployments[0]?.id ?? null);
	const streaming = $derived(app.deployments.some((d) => d.status === 'queued' || d.status === 'building'));

	$effect(() => {
		const inFlightId = inFlightDeployment?.id;
		if (!inFlightId || selected === inFlightId) return;
		selected = inFlightId;
		logsOpen = true;
		keepLogsOpen = false;
	});

	const historyPageSize = 5;
	let historyPage = $state(1);
	let historyAppId = $state<string | null>(null);
	const historyPageCount = $derived(Math.max(1, Math.ceil(app.deployments.length / historyPageSize)));
	const historyStart = $derived((historyPage - 1) * historyPageSize);
	const historyEnd = $derived(Math.min(historyStart + historyPageSize, app.deployments.length));
	const visibleDeployments = $derived(app.deployments.slice(historyStart, historyEnd));

	$effect(() => {
		const appId = app.id;
		const pageCount = historyPageCount;
		if (historyAppId !== appId) {
			historyAppId = appId;
			historyPage = 1;
		} else if (historyPage > pageCount) {
			historyPage = pageCount;
		}
	});

	function setHistoryPage(page: number) {
		historyPage = Math.max(1, Math.min(page, historyPageCount));
	}
	let rollbackTarget = $state<Deployment | null>(null);
	let confirmOpen = $state(false);
	let busy = $state(false);
	type DeploymentMethod = Exclude<DeployType, 'draft'>;
	let method = $state<DeploymentMethod>('image');
	let configuredType = $state<DeployType | null>(null);
	let image = $state('');
	let repo = $state('');
	let branch = $state('main');
	let buildMethod = $state('dockerfile');
	let credential = $state('');
	let compose = $state('services:\n  web:\n    image: nginx:alpine\n');
	let composeLoaded = $state(false);
	let service = $state('');
	let configBusy = $state(false);
	let configError = $state('');

	function syncConfig() {
		method = app.deploy_type === 'draft' ? 'image' : app.deploy_type;
		configuredType = app.deploy_type;
		image = app.image_name ?? '';
		repo = app.git_repo_url ?? '';
		branch = app.git_branch ?? 'main';
		buildMethod = app.build_method ?? 'dockerfile';
		service = app.compose_service ?? '';
		credential = '';
	}

	$effect(() => {
		if (app.deploy_type !== configuredType) syncConfig();
	});

	$effect(() => {
		if (app.deploy_type !== 'compose') {
			composeLoaded = false;
			return;
		}
		if (composeLoaded) return;
		composeLoaded = true;
		get<{ compose_yaml: string }>(`/apps/${app.id}/compose`)
			.then((file) => (compose = file.compose_yaml))
			.catch(() => {});
	});

	async function saveDeploymentConfig(e: SubmitEvent) {
		e.preventDefault();
		configBusy = true;
		configError = '';
		try {
			const body: Record<string, unknown> = { deploy_type: method };
			if (method === 'image') body.image_name = image.trim();
			if (method === 'git') {
				Object.assign(body, { git_repo_url: repo.trim(), git_branch: branch.trim(), build_method: buildMethod });
				if (credential.trim()) body.git_credential = credential.trim();
			}
			if (method === 'compose') {
				body.compose_yaml = compose;
				if (service.trim()) body.compose_service = service.trim();
			}
			const result = await post<{ redeploy_required: boolean }>(`/apps/${app.id}/deployment`, body);
			notify(result.redeploy_required ? 'Deployment method saved. Redeploy to apply it.' : 'Deployment method saved.');
			onchange();
		} catch (e) {
			configError = errorMessage(e);
		} finally {
			configBusy = false;
		}
	}

	let hook = $state<{ path: string; secret: string; branch: string | null } | null>(null);
	let panelDomain = $state<string | null>(null);
	let reveal = $state(false);
	$effect(() => {
		if (app.deploy_type === 'git' && !hook) {
			get<{ path: string; secret: string; branch: string | null }>(`/apps/${app.id}/webhook`).then((h) => (hook = h)).catch(() => {});
			get<Settings>('/settings').then((s) => (panelDomain = s.panel_domain)).catch(() => {});
		}
	});
	// Git hosts must reach the panel from the internet: prefer the public panel domain.
	const hookUrl = $derived(hook ? `${panelDomain ? `https://${panelDomain}` : location.origin}${hook.path}` : '');
	const reachable = $derived(!!panelDomain || !/^(localhost|127\.|\[::1\])/.test(location.hostname));

	const canRollback = (d: Deployment) =>
		app.deploy_type !== 'compose' &&
		!!d.image_ref &&
		(d.status === 'healthy' || d.status === 'rolled_back') &&
		d.id !== app.active_deployment_id;

	async function rollback() {
		if (!rollbackTarget) return;
		busy = true;
		try {
			const r = await post<{ deployment_id: string }>(`/deployments/${rollbackTarget.id}/rollback`);
			notify(`Rolling back to #${shortId(rollbackTarget.id)}`);
			selected = r.deployment_id;
			confirmOpen = false;
			onchange();
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			busy = false;
		}
	}

	function source(d: Deployment): string {
		if (d.commit_sha) return shortSha(d.commit_sha);
		if (app.deploy_type === 'compose') return 'compose stack';
		return d.image_ref ?? '-';
	}

	$effect(() => {
		if (streaming) {
			logsOpen = true;
			keepLogsOpen = false;
		} else if (!keepLogsOpen) {
			logsOpen = false;
		}
	});

	function toggleLogs() {
		if (streaming) return;
		logsOpen = !logsOpen;
		keepLogsOpen = logsOpen;
	}

	function selectDeployment(id: string) {
		selected = id;
		logsOpen = true;
		keepLogsOpen = true;
	}
</script>

{#if current}
	<section class="mb-6">
		<button
			type="button"
			class="flex w-full items-center gap-2 border-b border-line px-4 py-3 text-left text-[13px] text-accent-text hover:bg-hover focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent-text"
			aria-expanded={logsOpen}
			aria-controls="build-logs-panel"
			onclick={toggleLogs}
		>
			<Icon name="chevron_right" size={18} class={logsOpen ? 'rotate-90 transition-transform' : 'transition-transform'} />
			<span>{logsOpen ? 'Hide Build Logs' : 'View Build Logs'}</span>
			{#if streaming}<span class="text-ok">Streaming</span>{/if}
			<span class="flex-1"></span>
			<span class="mono mr-4 text-muted">#{shortId(current)}</span>
		</button>
		{#if logsOpen}
			<div id="build-logs-panel" class="panel mt-3 overflow-hidden">
				<div class="p-5">
					<LogView path={`/deployments/${current}/logs`} height="24rem" empty="No output yet." />
				</div>
			</div>
		{/if}
	</section>
{/if}

<section class="mb-6">
	<div class="mb-3">
		<h2 class="text-[14px] font-semibold">Build history</h2>
	</div>

	<ul class="panel divide-y divide-line sm:hidden">
	{#each visibleDeployments as d (d.id)}
		<li class="flex items-center gap-3 px-4 py-3 text-[13px] {current === d.id ? 'bg-accent-soft' : ''}">
			<div class="min-w-0 flex-1">
				<div class="flex items-center gap-2">
					<Status status={d.status} size="sm" />
					<span class="mono">#{shortId(d.id)}</span>
					{#if d.id === app.active_deployment_id}<span class="rounded-md bg-ok-soft px-1.5 text-[11px] font-medium text-ok">Live</span>{/if}
				</div>
				<div class="mt-0.5 truncate text-xs text-muted">
					{ago(d.started_at)} · {d.finished_at ? duration(d.started_at, d.finished_at) : 'running'} · {source(d)}
				</div>
			</div>
			<button class="btn btn-ghost btn-sm" aria-pressed={current === d.id} onclick={() => selectDeployment(d.id)}><Icon name="description" size={17} /> Log</button>
			{#if canRollback(d)}
				<button class="btn btn-sm" aria-label="Roll back to #{shortId(d.id)}" onclick={() => { rollbackTarget = d; confirmOpen = true; }}><Icon name="undo" size={17} /></button>
			{/if}
		</li>
	{:else}
		<li class="px-4 py-8 text-center text-muted">No deployments yet. Press Deploy to ship the first one.</li>
	{/each}
	</ul>

	<div class="panel hidden overflow-x-auto sm:block">
		<table class="table min-w-[680px]">
		<thead>
			<tr><th>Status</th><th>Deployment</th><th>Started</th><th>Took</th><th>Source</th><th class="w-0"></th></tr>
		</thead>
		<tbody>
			{#each visibleDeployments as d (d.id)}
				<tr class={current === d.id ? 'bg-accent-soft' : ''}>
					<td><Status status={d.status} /></td>
					<td>
						<span class="mono">#{shortId(d.id)}</span>
						{#if d.id === app.active_deployment_id}<span class="ml-2 rounded-md bg-ok-soft px-1.5 py-0.5 text-[11px] font-medium text-ok">Active</span>{/if}
					</td>
					<td title={dateTime(d.started_at)}>{ago(d.started_at)}</td>
					<td class="text-muted">{d.finished_at ? duration(d.started_at, d.finished_at) : '…'}</td>
					<td class="mono max-w-[320px] truncate text-muted" title={d.image_ref ?? ''}>{source(d)}</td>
					<td>
						<div class="flex justify-end gap-1">
						<button class="btn btn-ghost btn-sm" aria-pressed={current === d.id} onclick={() => selectDeployment(d.id)}>
								<Icon name="description" size={17} /> Log
							</button>
							{#if canRollback(d)}
								<button class="btn btn-sm" onclick={() => { rollbackTarget = d; confirmOpen = true; }}>
									<Icon name="undo" size={17} /> Roll back
								</button>
							{/if}
						</div>
					</td>
				</tr>
			{:else}
				<tr><td colspan="6" class="py-10 text-center text-muted">No deployments yet. Press Deploy to ship the first one.</td></tr>
			{/each}
		</tbody>
		</table>
	</div>
	{#if app.deployments.length > historyPageSize}
		<div class="mt-3 flex flex-wrap items-center justify-between gap-3">
			<p class="text-xs text-muted">Showing {historyStart + 1}–{historyEnd} of {app.deployments.length} builds</p>
			<nav class="flex items-center gap-1" aria-label="Build history pagination">
				<button
					type="button"
					class="btn btn-ghost btn-sm"
					disabled={historyPage === 1}
					aria-label="Previous build history page"
					onclick={() => setHistoryPage(historyPage - 1)}
				>
					<Icon name="chevron_right" size={17} class="rotate-180" /> Previous
				</button>
				<span class="px-2 text-xs text-muted" aria-live="polite">Page {historyPage} of {historyPageCount}</span>
				<button
					type="button"
					class="btn btn-ghost btn-sm"
					disabled={historyPage === historyPageCount}
					aria-label="Next build history page"
					onclick={() => setHistoryPage(historyPage + 1)}
				>
					Next <Icon name="chevron_right" size={17} />
				</button>
			</nav>
		</div>
	{/if}
</section>

<section class="panel mb-6">
	<div class="panel-head"><h2>Deployment method</h2></div>
	<form class="grid gap-4 p-5" onsubmit={saveDeploymentConfig}>
		<label>
			<span class="label">Method</span>
			<select class="input" bind:value={method}>
				<option value="image">Docker image</option>
				<option value="git">Git repository</option>
				<option value="compose">Compose stack</option>
			</select>
		</label>

		{#if method === 'image'}
			<label>
				<span class="label">Image</span>
				<input class="input mono" bind:value={image} required placeholder="nginx:latest" spellcheck="false" />
			</label>
		{:else if method === 'git'}
			<div class="grid gap-4 sm:grid-cols-2">
				<label class="sm:col-span-2">
					<span class="label">Repository URL</span>
					<input class="input mono" bind:value={repo} required placeholder="https://github.com/example/app.git" spellcheck="false" />
				</label>
				<label>
					<span class="label">Branch</span>
					<input class="input mono" bind:value={branch} required />
				</label>
				<label>
					<span class="label">Build with</span>
					<select class="input" bind:value={buildMethod}>
						<option value="dockerfile">Dockerfile</option>
						<option value="railpack">Railpack</option>
					</select>
				</label>
				<label class="sm:col-span-2">
					<span class="label">Access token or deploy key <span class="font-normal">(optional)</span></span>
					<textarea class="input mono" rows="2" bind:value={credential} placeholder="Leave empty to keep the stored credential" autocomplete="off" spellcheck="false"></textarea>
				</label>
			</div>
		{:else}
			<label>
				<span class="label">docker-compose.yml</span>
				<textarea class="input mono" rows="8" bind:value={compose} required spellcheck="false"></textarea>
			</label>
			<label>
				<span class="label">Exposed service <span class="font-normal">(optional)</span></span>
				<input class="input mono" bind:value={service} placeholder="web" />
			</label>
		{/if}

		{#if configError}<p role="alert" class="rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">{configError}</p>{/if}
		<div class="flex justify-end">
			<button class="btn btn-primary" disabled={configBusy}>
				{#if configBusy}<Icon name="progress_activity" size={18} class="animate-spin motion-reduce:animate-none" />{/if}
				{configBusy ? 'Saving…' : 'Save deployment method'}
			</button>
		</div>
	</form>
	{#if app.deploy_type === 'git' && hook}
		<div class="border-t border-line p-5">
			<h2 class="text-[14px] font-semibold">Deploy on push</h2>
			<p class="mt-1 text-[13px] text-muted">Pushes to <code>{hook.branch}</code> deploy automatically.</p>
			{#if !reachable}
				<p class="mt-3 rounded-md border border-warn/40 bg-warn-soft px-3 py-2 text-[13px]">
					<span class="font-medium text-warn">Your git host can't reach this URL.</span>
					<a class="link" href="/settings">Set a panel domain</a>
				</p>
			{/if}
			<div class="mt-3 grid gap-3 lg:grid-cols-2">
				<div>
					<span class="label">Payload URL</span>
					<div class="flex items-center gap-1">
						<code class="input flex items-center overflow-x-auto whitespace-nowrap">{hookUrl}</code>
						<Copy value={hookUrl} label="Copy payload URL" />
					</div>
				</div>
				<div>
					<span class="label">Secret <span class="font-normal">(GitHub/Gitea secret, GitLab secret token)</span></span>
					<div class="flex items-center gap-1">
						<code class="input flex items-center overflow-x-auto whitespace-nowrap">{reveal ? hook.secret : '•'.repeat(24)}</code>
						<button class="btn btn-ghost btn-sm" onclick={() => (reveal = !reveal)} aria-label={reveal ? 'Hide secret' : 'Show secret'}>
							{#if reveal}<Icon name="visibility_off" size={17} />{:else}<Icon name="visibility" size={17} />{/if}
						</button>
						<Copy value={hook.secret} label="Copy secret" />
					</div>
				</div>
			</div>
		</div>
	{/if}
</section>

<Confirm bind:open={confirmOpen} title="Roll back?" confirmLabel="Roll back" busy={busy} onconfirm={rollback}>
	{#if rollbackTarget}
		Redeploy the image of <span class="mono">#{shortId(rollbackTarget.id)}</span>
		({source(rollbackTarget)}) with zero downtime. No rebuild; current environment variables and domains apply.
	{/if}
</Confirm>
