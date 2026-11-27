<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import { get, post, patch, del, errorMessage, type AppDetail, type BackupJob, type BackupRun, type S3Target } from '#lib/api.ts';
	import { ago, bytes, dateTime, describeCron, until } from '#lib/format.ts';
	import type { AppSummary } from '#lib/types-board.ts';
	import { notify } from '#lib/toast.svelte.ts';
	import Status from '#lib/components/Status.svelte';
	import Confirm from '#lib/components/Confirm.svelte';
	import Skeleton from '#lib/components/Skeleton.svelte';

	let jobs = $state<BackupJob[]>([]);
	let targets = $state<S3Target[]>([]);
	let apps = $state<AppSummary[]>([]);
	let runs = $state<Record<string, BackupRun[]>>({});
	let openRuns = $state<string | null>(null);
	let loaded = $state(false);
	let loadError = $state('');

	async function refresh() {
		try {
			[jobs, targets, apps] = await Promise.all([get<BackupJob[]>('/backups'), get<S3Target[]>('/s3-targets'), get<AppSummary[]>('/apps')]);
			loadError = '';
			if (openRuns) runs[openRuns] = await get<BackupRun[]>(`/backups/${openRuns}/runs`);
		} catch (e) {
			loadError = errorMessage(e);
		} finally {
			loaded = true;
		}
	}
	onMount(() => {
		refresh();
		const t = setInterval(refresh, 8000);
		return () => clearInterval(t);
	});

	const appName = (id: string) => apps.find((a) => a.id === id)?.name ?? 'deleted app';
	const targetName = (id: string) => targets.find((t) => t.id === id)?.name ?? '?';
	const driverLabel = { postgres: 'PostgreSQL dump', mysql: 'MySQL dump', volume: 'Volume archive' };

	async function run(job: BackupJob) {
		try {
			await post(`/backups/${job.id}/run`);
			notify(`Backup of ${appName(job.app_id)} started.`);
			await refresh();
		} catch (e) {
			notify(errorMessage(e), 'bad');
		}
	}
	async function toggle(job: BackupJob) {
		await patch(`/backups/${job.id}`, { enabled: !job.enabled }).catch((e) => notify(errorMessage(e), 'bad'));
		await refresh();
	}
	// One confirmation dialog for every destructive action on this page.
	let pending = $state<{ title: string; body: string; label: string; run: () => Promise<unknown> } | null>(null);
	let confirmOpen = $state(false);
	let confirmBusy = $state(false);
	function ask(title: string, body: string, run: () => Promise<unknown>, label = 'Delete') {
		pending = { title, body, label, run };
		confirmOpen = true;
	}
	function restore(job: BackupJob, r: BackupRun) {
		const what = job.driver === 'volume' ? 'volume' : 'database';
		ask(
			`Restore ${appName(job.app_id)} from this backup?`,
			`The ${what} is replaced with the backup taken ${ago(r.started_at)}. Changes made since then are lost.` +
				(job.driver === 'volume' ? ' The app is stopped during the restore and started again afterwards.' : ''),
			async () => {
				await post(`/backups/${job.id}/runs/${r.id}/restore`);
				notify(`Restored ${appName(job.app_id)}.`);
			},
			'Restore'
		);
	}
	async function confirmed() {
		if (!pending) return;
		confirmBusy = true;
		await pending.run().catch((e) => notify(errorMessage(e), 'bad'));
		confirmBusy = false;
		confirmOpen = false;
		await refresh();
	}
	function removeJob(job: BackupJob) {
		ask(
			'Delete backup job?',
			`The ${driverLabel[job.driver]} job for ${appName(job.app_id)} stops running. Backups already in the bucket are kept.`,
			() => del(`/backups/${job.id}`)
		);
	}
	async function showRuns(job: BackupJob) {
		openRuns = openRuns === job.id ? null : job.id;
		if (openRuns) runs[job.id] = await get<BackupRun[]>(`/backups/${job.id}/runs`);
	}

	// New job form
	let showJob = $state(false);
	let j = $state({ app_id: '', s3_target_id: '', driver: 'postgres', cron_expr: '30 2 * * *', retention_count: 7, volume: '', db_name: '', db_user: '', db_password: '' });
	let volumes = $state<string[]>([]);
	let jobError = $state('');
	$effect(() => {
		if (j.driver === 'volume' && j.app_id) {
			get<AppDetail>(`/apps/${j.app_id}`).then((a) => {
				volumes = a.volumes.map((v) => v.name);
				j.volume = volumes[0] ?? '';
			});
		}
	});
	async function createJob(e: SubmitEvent) {
		e.preventDefault();
		jobError = '';
		const body: Record<string, unknown> = { app_id: j.app_id, s3_target_id: j.s3_target_id, driver: j.driver, cron_expr: j.cron_expr, retention_count: Number(j.retention_count) };
		if (j.driver === 'volume') body.volume = j.volume;
		else for (const k of ['db_name', 'db_user', 'db_password'] as const) if (j[k].trim()) body[k] = j[k].trim();
		try {
			await post('/backups', body);
			showJob = false;
			notify('Backup job created.');
			await refresh();
		} catch (err) {
			jobError = errorMessage(err);
		}
	}

	// New target form
	let showTarget = $state(false);
	let t = $state({ name: '', endpoint: 'https://', bucket: '', region: 'us-east-1', access_key: '', secret_key: '' });
	let targetError = $state('');
	async function createTarget(e: SubmitEvent) {
		e.preventDefault();
		targetError = '';
		try {
			await post('/s3-targets', t);
			showTarget = false;
			t = { name: '', endpoint: 'https://', bucket: '', region: 'us-east-1', access_key: '', secret_key: '' };
			notify('Storage target added.');
			await refresh();
		} catch (err) {
			targetError = errorMessage(err);
		}
	}
	async function test(target: S3Target) {
		try {
			await post(`/s3-targets/${target.id}/test`);
			notify(`${target.name}: credentials and bucket OK.`);
		} catch (e) {
			notify(errorMessage(e), 'bad');
		}
		await refresh();
	}
	function removeTarget(target: S3Target) {
		ask('Remove storage target?', `${target.name} is removed from forgenerd. Objects in the bucket are kept.`, () =>
			del(`/s3-targets/${target.id}`)
		);
	}
</script>

<div class="flex flex-wrap items-center justify-between gap-4">
	<h1 class="page-title">Backups</h1>
	<button class="btn btn-primary" disabled={targets.length === 0 || apps.length === 0} title={targets.length === 0 ? 'Add a storage target first' : ''} onclick={() => { showJob = !showJob; j.app_id ||= apps[0]?.id ?? ''; j.s3_target_id ||= targets[0]?.id ?? ''; }}>
		<Icon name="add" size={19} /> New backup job
	</button>
</div>

{#if showJob}
	<form class="panel mt-6 grid gap-4 p-5 sm:grid-cols-3" onsubmit={createJob}>
		<label><span class="label">App</span><select class="input" bind:value={j.app_id}>{#each apps as a (a.id)}<option value={a.id}>{a.name}</option>{/each}</select></label>
		<label><span class="label">What to back up</span>
			<select class="input" bind:value={j.driver}><option value="postgres">PostgreSQL dump</option><option value="mysql">MySQL dump</option><option value="volume">Volume archive</option></select>
		</label>
		<label><span class="label">Storage target</span><select class="input" bind:value={j.s3_target_id}>{#each targets as tg (tg.id)}<option value={tg.id}>{tg.name}</option>{/each}</select></label>
		<label><span class="label">Schedule (cron, UTC)</span><input class="input mono" bind:value={j.cron_expr} placeholder="30 2 * * *" required />{#if describeCron(j.cron_expr)}<span class="mt-1 block text-xs text-muted">{describeCron(j.cron_expr)}</span>{/if}</label>
		<label><span class="label">Keep last</span><input class="input" type="number" min="1" max="365" bind:value={j.retention_count} /></label>
		{#if j.driver === 'volume'}
			<label><span class="label">Volume</span>
				<select class="input" bind:value={j.volume} required>{#each volumes as v (v)}<option value={v}>{v}</option>{:else}<option value="" disabled>This app has no volumes</option>{/each}</select>
			</label>
		{:else}
			<label><span class="label">Database <span class="font-normal">(optional)</span></span><input class="input mono" bind:value={j.db_name} placeholder="from container env" /></label>
			<label><span class="label">User <span class="font-normal">(optional)</span></span><input class="input mono" bind:value={j.db_user} placeholder="from container env" /></label>
			<label><span class="label">Password <span class="font-normal">(optional)</span></span><input class="input" type="password" bind:value={j.db_password} autocomplete="off" placeholder="from container env" /></label>
		{/if}
		{#if jobError}<p role="alert" class="text-[13px] text-bad sm:col-span-3">{jobError}</p>{/if}
		<div class="flex gap-2 sm:col-span-3"><button class="btn btn-primary">Create job</button><button type="button" class="btn btn-ghost" onclick={() => (showJob = false)}>Cancel</button></div>
	</form>
{/if}

{#if !loaded}
	<section class="panel mt-6 overflow-hidden" aria-busy="true" aria-label="Loading backup jobs">
		<div class="grid grid-cols-[minmax(8rem,1fr)_8rem_minmax(12rem,1fr)_4rem_minmax(10rem,1fr)_8rem] gap-4 border-b border-line px-5 py-3">
			{#each Array(6) as _, i (i)}<Skeleton class="h-3 w-16" />{/each}
		</div>
		{#each Array(4) as _, i (i)}
			<div class="grid grid-cols-[minmax(8rem,1fr)_8rem_minmax(12rem,1fr)_4rem_minmax(10rem,1fr)_8rem] items-center gap-4 border-b border-line px-5 py-4 last:border-b-0">
				<Skeleton class="h-4 w-24" /><Skeleton class="h-3 w-28" /><Skeleton class="h-3 w-36" /><Skeleton class="h-3 w-5" /><Skeleton class="h-3 w-24" /><Skeleton class="ml-auto h-8 w-20" />
			</div>
		{/each}
	</section>
{:else if loadError}
	<p class="mt-6 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad" role="alert">Could not load backups: {loadError}</p>
{:else}
<ul class="panel mt-6 divide-y divide-line sm:hidden">
	{#each jobs as job (job.id)}
		<li class="px-4 py-3 text-[13px] {job.last_run?.status === 'failed' ? 'bg-bad-soft' : ''}">
			<div class="flex items-center justify-between gap-2">
				<span class="font-medium">{appName(job.app_id)} · {driverLabel[job.driver]}</span>
				{#if job.last_run}<Status status={job.last_run.status === 'running' ? 'running_job' : job.last_run.status} size="sm" />{/if}
			</div>
			<div class="mt-0.5 text-xs text-muted">
				{job.enabled ? `${describeCron(job.cron_expr) ?? job.cron_expr} · next ${until(job.next_run_at)}` : 'Paused'} · keep {job.retention_count}
			</div>
			<div class="mt-0.5 text-xs text-muted">
				Last run: {#if job.last_run}{job.last_run.status === 'running' ? 'running now' : job.last_run.status} {ago(job.last_run.started_at)}{job.last_run.size_bytes != null ? ` · ${bytes(job.last_run.size_bytes)}` : ''}{:else}never{/if}
			</div>
			{#if job.last_run?.error}<div class="mt-0.5 truncate text-xs text-bad">{job.last_run.error}</div>{/if}
			<div class="mt-2 flex flex-wrap gap-1">
				<button class="btn btn-sm" onclick={() => run(job)}><Icon name="play_arrow" size={16} /> Run now</button>
				<button class="btn btn-ghost btn-sm" aria-pressed={openRuns === job.id} onclick={() => showRuns(job)}><Icon name="history" size={17} /> History</button>
				<button class="btn btn-ghost btn-sm" onclick={() => toggle(job)}>{job.enabled ? 'Pause' : 'Resume'}</button>
				<button class="btn btn-ghost btn-sm text-bad" onclick={() => removeJob(job)}><Icon name="delete" size={17} /> Delete</button>
			</div>
			{#if openRuns === job.id}
				<ul class="mt-2 divide-y divide-line rounded-md border border-line bg-raised text-xs">
					{#each runs[job.id] ?? [] as r (r.id)}
						<li class="flex items-center justify-between gap-2 px-3 py-2">
							<Status status={r.status === 'running' ? 'running_job' : r.status} size="sm" />
							<span class="text-muted">{ago(r.started_at)} · {r.trigger} · {bytes(r.size_bytes)}</span>
							{#if r.status === 'succeeded'}<button class="btn btn-ghost btn-sm" onclick={() => restore(job, r)}><Icon name="history" size={16} /> Restore</button>{/if}
						</li>
					{:else}<li class="px-3 py-2 text-muted">No runs yet.</li>{/each}
				</ul>
			{/if}
		</li>
	{:else}
		<li class="px-4 py-8 text-center text-muted">{loaded ? 'No backup jobs yet.' : 'Loading…'}</li>
	{/each}
</ul>

<section class="panel mt-6 hidden overflow-x-auto sm:block">
	<table class="table min-w-[880px]">
		<thead><tr><th>App</th><th>What</th><th>Schedule</th><th>Keep</th><th>Last run</th><th class="w-0"></th></tr></thead>
		<tbody>
			{#each jobs as job (job.id)}
				<tr class={job.last_run?.status === 'failed' ? 'bg-bad-soft' : ''}>
					<td><a class="link" href="/apps/{job.app_id}">{appName(job.app_id)}</a></td>
					<td>{driverLabel[job.driver]}<div class="text-xs text-muted">to {targetName(job.s3_target_id)}</div></td>
					<td title={job.cron_expr}>
						{#if job.enabled}
							<div>{describeCron(job.cron_expr) ?? job.cron_expr}</div>
							<div class="text-xs text-muted">Next run {until(job.next_run_at)}</div>
						{:else}<span class="text-muted">Paused</span>{/if}
					</td>
					<td class="tabular-nums">{job.retention_count}</td>
					<td>
						{#if job.last_run}
							<Status status={job.last_run.status === 'running' ? 'running_job' : job.last_run.status} size="sm" />
							<div class="text-xs text-muted">{ago(job.last_run.started_at)}{job.last_run.size_bytes != null ? ` · ${bytes(job.last_run.size_bytes)}` : ''}</div>
							{#if job.last_run.error}<div class="mt-0.5 max-w-[280px] truncate text-xs text-bad" title={job.last_run.error}>{job.last_run.error}</div>{/if}
						{:else}<span class="text-muted">Never run</span>{/if}
					</td>
					<td>
						<div class="flex justify-end gap-1">
							<button class="btn btn-sm" onclick={() => run(job)}><Icon name="play_arrow" size={16} /> Run now</button>
							<button class="btn btn-ghost btn-sm" aria-pressed={openRuns === job.id} onclick={() => showRuns(job)}><Icon name="history" size={17} /> History</button>
							<button class="btn btn-ghost btn-sm" onclick={() => toggle(job)}>
								{#if job.enabled}<Icon name="pause" size={17} /> Pause{:else}<Icon name="play_arrow" size={17} /> Resume{/if}
							</button>
							<button class="btn btn-ghost btn-sm btn-icon text-bad" aria-label="Delete job" onclick={() => removeJob(job)}><Icon name="delete" size={17} /></button>
						</div>
					</td>
				</tr>
				{#if openRuns === job.id}
					<tr>
						<td colspan="6" class="bg-raised">
							<table class="table">
								<thead><tr><th>Status</th><th>Started</th><th>Trigger</th><th>Size</th><th>Object</th><th class="w-0"></th></tr></thead>
								<tbody>
									{#each runs[job.id] ?? [] as r (r.id)}
										<tr>
											<td><Status status={r.status === 'running' ? 'running_job' : r.status} size="sm" /></td>
											<td title={dateTime(r.started_at)}>{ago(r.started_at)}</td>
											<td class="text-muted">{r.trigger}</td>
											<td class="tabular-nums">{bytes(r.size_bytes)}</td>
											<td class="mono max-w-[420px] truncate text-xs text-muted" title={r.error ?? r.object_key ?? ''}>{r.error ?? r.object_key ?? '-'}</td>
											<td>{#if r.status === 'succeeded'}<button class="btn btn-ghost btn-sm" onclick={() => restore(job, r)}><Icon name="history" size={16} /> Restore</button>{/if}</td>
										</tr>
									{:else}<tr><td colspan="6" class="text-muted">No runs yet.</td></tr>{/each}
								</tbody>
							</table>
						</td>
					</tr>
				{/if}
			{:else}
				<tr><td colspan="6" class="py-10 text-center text-muted">{loaded ? (targets.length === 0 ? 'Add a storage target first.' : 'No backup jobs yet.') : 'Loading…'}</td></tr>
			{/each}
		</tbody>
	</table>
</section>
{/if}

<div class="mt-10 flex flex-wrap items-center justify-between gap-4">
	<h2 class="text-[15px] font-semibold">Storage targets</h2>
	<button class="btn" onclick={() => (showTarget = !showTarget)}><Icon name="add" size={19} /> Add target</button>
</div>

{#if showTarget}
	<form class="panel mt-4 grid gap-4 p-5 sm:grid-cols-3" onsubmit={createTarget}>
		<label><span class="label">Name</span><input class="input" bind:value={t.name} required placeholder="r2-backups" /></label>
		<label class="sm:col-span-2"><span class="label">Endpoint</span><input class="input mono" bind:value={t.endpoint} required placeholder="https://<account>.r2.cloudflarestorage.com" /></label>
		<label><span class="label">Bucket</span><input class="input mono" bind:value={t.bucket} required /></label>
		<label><span class="label">Region</span><input class="input mono" bind:value={t.region} placeholder="us-east-1 / auto" required /></label>
		<span></span>
		<label><span class="label">Access key ID</span><input class="input mono" bind:value={t.access_key} required autocomplete="off" /></label>
		<label><span class="label">Secret access key</span><input class="input" type="password" bind:value={t.secret_key} required autocomplete="off" /></label>
		{#if targetError}<p role="alert" class="text-[13px] text-bad sm:col-span-3">{targetError}</p>{/if}
		<div class="flex gap-2 sm:col-span-3"><button class="btn btn-primary">Add target</button><button type="button" class="btn btn-ghost" onclick={() => (showTarget = false)}>Cancel</button></div>
	</form>
{/if}

<ul class="panel mt-4 divide-y divide-line sm:hidden">
	{#each targets as tg (tg.id)}
		<li class="px-4 py-3 text-[13px]">
			<div class="flex items-center justify-between gap-2">
				<span class="font-medium">{tg.name}</span>
				{#if tg.last_test_at}<Status status={tg.last_test_ok ? 'succeeded' : 'failed'} size="sm" />{/if}
			</div>
			<div class="mono mt-0.5 truncate text-xs text-muted">{tg.endpoint} · {tg.bucket} · {tg.region}</div>
			<div class="mt-0.5 text-xs text-muted">{tg.last_test_at ? `Tested ${ago(tg.last_test_at)}` : 'Not tested yet'}</div>
			{#if tg.last_test_error}<div class="mt-0.5 truncate text-xs text-bad">{tg.last_test_error}</div>{/if}
			<div class="mt-2 flex gap-1">
				<button class="btn btn-sm" onclick={() => test(tg)}><Icon name="cable" size={17} /> Test</button>
				<button class="btn btn-ghost btn-sm text-bad" onclick={() => removeTarget(tg)}><Icon name="delete" size={17} /> Remove</button>
			</div>
		</li>
	{:else}<li class="px-4 py-8 text-center text-muted">No storage targets.</li>{/each}
</ul>

<section class="panel mt-4 hidden overflow-x-auto sm:block">
	<table class="table min-w-[640px]">
		<thead><tr><th>Name</th><th>Endpoint · bucket · region</th><th>Last test</th><th class="w-0"></th></tr></thead>
		<tbody>
			{#each targets as tg (tg.id)}
				<tr>
					<td class="font-medium">{tg.name}</td>
					<td class="mono text-xs">
						<div>{tg.endpoint}</div>
						<div class="text-muted">{tg.bucket} · {tg.region}</div>
					</td>
					<td>
						{#if tg.last_test_at}
							<Status status={tg.last_test_ok ? 'succeeded' : 'failed'} size="sm" />
							<div class="text-xs text-muted">{ago(tg.last_test_at)}</div>
							{#if tg.last_test_error}<div class="max-w-[320px] truncate text-xs text-bad" title={tg.last_test_error}>{tg.last_test_error}</div>{/if}
						{:else}<span class="text-muted">Not tested yet</span>{/if}
					</td>
					<td><div class="flex justify-end gap-1">
						<button class="btn btn-sm" onclick={() => test(tg)}><Icon name="cable" size={17} /> Test</button>
						<button class="btn btn-ghost btn-sm btn-icon text-bad" aria-label="Remove {tg.name}" onclick={() => removeTarget(tg)}><Icon name="delete" size={17} /></button>
					</div></td>
				</tr>
			{:else}<tr><td colspan="4" class="py-8 text-center text-muted">No storage targets.</td></tr>{/each}
		</tbody>
	</table>
</section>

<Confirm bind:open={confirmOpen} title={pending?.title ?? ''} confirmLabel={pending?.label ?? 'Delete'} danger busy={confirmBusy} onconfirm={confirmed}>
	{pending?.body}
</Confirm>
