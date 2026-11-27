<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import {
		get,
		post,
		patch,
		errorMessage,
		type CleanupConfig,
		type CleanupReport,
		type DiskUsage,
		type PanelBackupConfig,
		type PanelBackupRun,
		type S3Target,
		type UpdateStatus
	} from '#lib/api.ts';
	import { ago, bytes, dateTime, describeCron } from '#lib/format.ts';
	import { notify } from '#lib/toast.svelte.ts';
	import Copy from '#lib/components/Copy.svelte';
	import Confirm from '#lib/components/Confirm.svelte';
	import Skeleton from '#lib/components/Skeleton.svelte';

	// Updates
	let update = $state<UpdateStatus | null>(null);
	let checking = $state(false);
	let updateError = $state('');
	const upgradeCommand = $derived(
		!update
			? ''
			: update.repo === 'forgenerd/forgenerd'
				? 'curl -fsSL https://raw.githubusercontent.com/forgenerd/forgenerd/main/scripts/install.sh | sudo bash'
				: `curl -fsSL https://raw.githubusercontent.com/${update.repo}/main/scripts/install.sh | sudo bash -s -- --repo ${update.repo}`
	);

	async function checkUpdates() {
		checking = true;
		updateError = '';
		try {
			update = await get<UpdateStatus>('/maintenance/updates');
		} catch (e) {
			updateError = errorMessage(e);
		} finally {
			checking = false;
		}
	}

	// Cleanup schedule
	let loaded = $state(false);
	let loadError = $state('');
	let scheduled = $state(false);
	let cron = $state('0 1 * * *');
	let keep = $state(3);
	let saving = $state(false);
	let saveError = $state('');
	const cronHint = $derived(describeCron(cron));

	// Disk usage
	let disk = $state<DiskUsage | null>(null);
	let diskError = $state('');
	async function loadDisk() {
		try {
			disk = await get<DiskUsage>('/maintenance/disk');
			diskError = '';
		} catch (e) {
			diskError = errorMessage(e);
		}
	}
	const used = $derived(disk ? disk.filesystem_total_bytes - disk.filesystem_free_bytes : 0);
	const usedShare = $derived(disk && disk.filesystem_total_bytes ? used / disk.filesystem_total_bytes : 0);

	// Panel backup
	let targets = $state<S3Target[]>([]);
	let backupTarget = $state('');
	let backupScheduled = $state(false);
	let backupCron = $state('0 3 * * *');
	let backupKeep = $state(14);
	let lastBackup = $state<PanelBackupRun | null>(null);
	let backupSaving = $state(false);
	let backupError = $state('');
	let backingUp = $state(false);
	const backupCronHint = $derived(describeCron(backupCron));

	function applyBackup(config: PanelBackupConfig) {
		backupTarget = config.s3_target_id ?? '';
		backupScheduled = config.cron_expr !== null;
		if (config.cron_expr) backupCron = config.cron_expr;
		backupKeep = config.retention_count;
		lastBackup = config.last_run;
	}

	async function saveBackup(e: SubmitEvent) {
		e.preventDefault();
		backupSaving = true;
		backupError = '';
		try {
			applyBackup(
				await patch<PanelBackupConfig>('/maintenance/panel-backup', {
					s3_target_id: backupTarget || null,
					cron_expr: backupScheduled ? backupCron.trim() : null,
					retention_count: Number(backupKeep)
				})
			);
			notify(backupScheduled ? 'Panel backup schedule saved.' : 'Panel backup settings saved.');
		} catch (err) {
			backupError = errorMessage(err);
		} finally {
			backupSaving = false;
		}
	}

	async function backupNow() {
		backingUp = true;
		try {
			lastBackup = await post<PanelBackupRun>('/maintenance/panel-backup/run');
			notify(lastBackup.ok ? 'Panel backed up.' : 'Panel backup failed.', lastBackup.ok ? 'ok' : 'bad');
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			backingUp = false;
		}
	}

	onMount(async () => {
		loadDisk();
		try {
			const [config, backup, nextTargets] = await Promise.all([
				get<CleanupConfig>('/maintenance/cleanup'),
				get<PanelBackupConfig>('/maintenance/panel-backup'),
				get<S3Target[]>('/s3-targets')
			]);
			apply(config);
			applyBackup(backup);
			targets = nextTargets;
		} catch (e) {
			loadError = errorMessage(e);
		} finally {
			loaded = true;
		}
	});

	function apply(config: CleanupConfig) {
		scheduled = config.cron_expr !== null;
		if (config.cron_expr) cron = config.cron_expr;
		keep = config.keep_images;
	}

	async function save(e: SubmitEvent) {
		e.preventDefault();
		saving = true;
		saveError = '';
		try {
			apply(
				await patch<CleanupConfig>('/maintenance/cleanup', {
					cron_expr: scheduled ? cron.trim() : null,
					keep_images: Number(keep)
				})
			);
			notify(scheduled ? 'Cleanup schedule saved.' : 'Automatic cleanup turned off.');
		} catch (err) {
			saveError = errorMessage(err);
		} finally {
			saving = false;
		}
	}

	// Manual cleanup
	let confirmOpen = $state(false);
	let cleaning = $state(false);
	let report = $state<CleanupReport | null>(null);

	async function cleanNow() {
		cleaning = true;
		try {
			report = await post<CleanupReport>('/maintenance/cleanup/run');
			notify(report.errors.length ? 'Cleanup finished with errors.' : 'Cleanup finished.', report.errors.length ? 'bad' : 'ok');
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			cleaning = false;
			confirmOpen = false;
		}
	}
</script>

<h1 class="page-title">Maintenance</h1>

<section class="panel mt-6">
	<div class="panel-head"><h2>Updates</h2></div>
	<div class="grid gap-3 p-5">
		<div class="flex flex-wrap items-center gap-3">
			<p class="flex-1 text-[13px]">
				{#if update}Running <code>forgenerd v{update.current}</code>{:else}Compare this server's forgenerd version with the latest release.{/if}
			</p>
			<button class="btn" onclick={checkUpdates} disabled={checking}>
				<Icon name={checking ? 'progress_activity' : 'refresh'} size={18} class={checking ? 'animate-spin motion-reduce:animate-none' : ''} />
				{checking ? 'Checking…' : 'Check for updates'}
			</button>
		</div>
		{#if updateError}
			<p role="alert" class="rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">Could not check for updates: {updateError}</p>
		{:else if update && update.update_available}
			<div class="rounded-md bg-warn-soft p-4 text-[13px]" role="status">
				<p class="font-medium">
					Version {update.latest} is available.
					{#if update.release_url}<a class="link inline-flex items-center gap-1" href={update.release_url} target="_blank" rel="noopener noreferrer">Release notes <Icon name="open_in_new" size={15} /></a>{/if}
				</p>
				<p class="mt-2 text-muted">Upgrade on the server by re-running the installer. Apps keep running; data and secrets are kept.</p>
				<div class="mt-2 flex items-center gap-1">
					<code class="input flex items-center overflow-x-auto bg-surface whitespace-nowrap">{upgradeCommand}</code>
					<Copy value={upgradeCommand} label="Copy upgrade command" />
				</div>
			</div>
		{:else if update}
			<p class="flex items-center gap-1.5 text-[13px] text-ok" role="status"><Icon name="check_circle" size={17} filled /> You are on the latest release ({update.latest}).</p>
		{/if}
	</div>
</section>

<section class="panel mt-6">
	<div class="panel-head"><h2>Disk</h2></div>
	<div class="grid gap-4 p-5">
		{#if diskError}
			<p role="alert" class="rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">Could not read disk usage: {diskError}</p>
		{:else if !disk}
			<div class="grid gap-2" aria-busy="true" aria-label="Loading disk usage"><Skeleton class="h-3 w-64" /><Skeleton class="h-2 w-full" /></div>
		{:else}
			<div>
				<div class="flex flex-wrap items-baseline justify-between gap-2 text-[13px]">
					<span><span class="font-medium tabular-nums">{bytes(disk.filesystem_free_bytes)}</span> free of {bytes(disk.filesystem_total_bytes)}</span>
					<span class="text-muted tabular-nums">{Math.round(usedShare * 100)}% used</span>
				</div>
				<div
					class="mt-2 h-2 overflow-hidden rounded-full bg-raised"
					role="meter"
					aria-label="Disk used"
					aria-valuemin={0}
					aria-valuemax={100}
					aria-valuenow={Math.round(usedShare * 100)}
				>
					<div class="h-full rounded-full {usedShare > 0.9 ? 'bg-bad' : usedShare > 0.75 ? 'bg-warn' : 'bg-accent'}" style="width: {Math.min(100, usedShare * 100)}%"></div>
				</div>
			</div>
			<dl class="grid grid-cols-2 gap-x-6 gap-y-2 text-[13px] sm:grid-cols-4">
				<div><dt class="text-xs text-muted">Images</dt><dd class="tabular-nums">{bytes(disk.images_bytes)}{#if disk.images_reclaimable_bytes}<span class="text-xs text-muted"> ({bytes(disk.images_reclaimable_bytes)} unused)</span>{/if}</dd></div>
				<div><dt class="text-xs text-muted">Build cache</dt><dd class="tabular-nums">{bytes(disk.build_cache_bytes)}</dd></div>
				<div><dt class="text-xs text-muted">Volumes</dt><dd class="tabular-nums">{bytes(disk.volumes_bytes)}</dd></div>
				<div><dt class="text-xs text-muted">Containers</dt><dd class="tabular-nums">{bytes(disk.containers_bytes)}</dd></div>
			</dl>
		{/if}
	</div>
</section>

{#if !loaded}
	<section class="panel mt-6 grid gap-4 p-5" aria-busy="true" aria-label="Loading cleanup settings">
		<div class="grid gap-2"><Skeleton class="h-4 w-44" /><Skeleton class="h-3 w-96 max-w-full" /></div>
		<div class="grid gap-2"><Skeleton class="h-3 w-24" /><Skeleton class="h-9 w-40" /></div>
		<div class="grid gap-2"><Skeleton class="h-3 w-24" /><Skeleton class="h-9 w-full" /></div>
	</section>
{:else if loadError}
	<p class="mt-6 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad" role="alert">Could not load cleanup settings: {loadError}</p>
{:else}
	<form class="panel mt-6" onsubmit={save}>
		<div class="panel-head"><h2>Disk cleanup</h2></div>
		<div class="grid gap-4 p-5">
			<p class="text-[13px] text-muted">
				Every build of a git app produces a new image. Most layers are shared with the previous build, but each deploy still adds
				some disk usage. Cleanup removes app images beyond the most recent ones kept for rollback, plus dangling images and unused
				build cache. The active deployment's image is never removed.
			</p>
			<label>
				<span class="label">Images kept per app</span>
				<input class="input w-32" type="number" min="1" max="50" required bind:value={keep} />
				<span class="mt-1 block text-xs text-muted">The newest healthy deployments you can roll back to. Also applied after every deploy.</span>
			</label>
			<label class="flex items-center gap-2 text-[13px]">
				<input type="checkbox" bind:checked={scheduled} />
				<span>Run cleanup automatically</span>
			</label>
			{#if scheduled}
				<label>
					<span class="label">Schedule (cron, UTC)</span>
					<input class="input mono max-w-xs" bind:value={cron} placeholder="0 1 * * *" required spellcheck="false" />
					<span class="mt-1 block text-xs text-muted">{cronHint ?? 'minute hour day-of-month month day-of-week'}. For example <code>0 1 * * *</code> runs every day at 01:00 UTC.</span>
				</label>
			{/if}
			{#if saveError}<p role="alert" class="text-[13px] text-bad">{saveError}</p>{/if}
		</div>
		<div class="flex justify-end border-t border-line px-5 py-3">
			<button class="btn btn-primary" disabled={saving}>{#if saving}<Icon name="progress_activity" size={18} class="animate-spin motion-reduce:animate-none" />{/if}{saving ? 'Saving…' : 'Save'}</button>
		</div>
	</form>

	<section class="panel mt-6">
		<div class="panel-head"><h2>Manual cleanup</h2></div>
		<div class="grid gap-3 p-5">
			<div class="flex flex-wrap items-center gap-3">
				<p class="flex-1 text-[13px] text-muted">Free disk space now, using the same rules as the schedule.</p>
				<button class="btn" onclick={() => (confirmOpen = true)} disabled={cleaning}><Icon name="delete" size={18} /> Clean up now</button>
			</div>
			{#if report}
				<div class="rounded-md border border-line p-4 text-[13px]" role="status">
					<p class="font-medium">
						Removed {report.removed_images.length} old image{report.removed_images.length === 1 ? '' : 's'} and freed {bytes(report.reclaimed_bytes)} of dangling images and build cache.
					</p>
					{#if report.removed_images.length}
						<ul class="mono mt-2 grid gap-0.5 text-xs text-muted">
							{#each report.removed_images as tag (tag)}<li>{tag}</li>{/each}
						</ul>
					{/if}
					{#if report.errors.length}
						<ul class="mt-2 grid gap-0.5 text-bad">
							{#each report.errors as err, i (i)}<li>{err}</li>{/each}
						</ul>
					{/if}
				</div>
			{/if}
		</div>
	</section>

	<form class="panel mt-6" onsubmit={saveBackup}>
		<div class="panel-head"><h2>Panel backup</h2></div>
		<div class="grid gap-4 p-5">
			<p class="text-[13px] text-muted">
				Backs up forgenerd's own database (apps, domains, environment variables, backup jobs and settings) to S3, so a lost
				server can be rebuilt. Secrets in it are encrypted with the master key, which is never uploaded. Keep a copy of
				<code>/var/lib/forgenerd/master.key</code> somewhere safe; without it the backup cannot be decrypted.
			</p>
			{#if targets.length === 0}
				<p class="text-[13px]">Add an S3 target on the <a class="link" href="/backups">Backups</a> page first.</p>
			{:else}
				<div class="grid gap-4 sm:grid-cols-2">
					<label>
						<span class="label">S3 target</span>
						<select class="input" bind:value={backupTarget}>
							<option value="">None</option>
							{#each targets as t (t.id)}<option value={t.id}>{t.name} ({t.bucket})</option>{/each}
						</select>
					</label>
					<label>
						<span class="label">Backups kept</span>
						<input class="input w-32" type="number" min="1" max="365" required bind:value={backupKeep} />
					</label>
				</div>
				<label class="flex items-center gap-2 text-[13px]">
					<input type="checkbox" bind:checked={backupScheduled} disabled={!backupTarget} />
					<span>Back up automatically</span>
				</label>
				{#if backupScheduled && backupTarget}
					<label>
						<span class="label">Schedule (cron, UTC)</span>
						<input class="input mono max-w-xs" bind:value={backupCron} placeholder="0 3 * * *" required spellcheck="false" />
						<span class="mt-1 block text-xs text-muted">{backupCronHint ?? 'minute hour day-of-month month day-of-week'}</span>
					</label>
				{/if}
				{#if lastBackup}
					<p class="text-[13px] {lastBackup.ok ? '' : 'text-bad'}" title={dateTime(lastBackup.finished_at)}>
						{#if lastBackup.ok}
							Last backup {ago(lastBackup.finished_at)}, {bytes(lastBackup.size_bytes)}: <code class="text-xs">{lastBackup.object_key}</code>
						{:else}
							Last backup failed {ago(lastBackup.finished_at)}: {lastBackup.error}
						{/if}
					</p>
				{/if}
				<p class="text-xs text-muted">
					To restore on a new server: install forgenerd, put the master key in place, stop the service, download the backup, run
					<code>sudo -u forgenerd forgenerd restore-db &lt;file&gt;</code>, then start the service.
				</p>
			{/if}
			{#if backupError}<p role="alert" class="text-[13px] text-bad">{backupError}</p>{/if}
		</div>
		{#if targets.length}
			<div class="flex justify-end gap-2 border-t border-line px-5 py-3">
				<button type="button" class="btn" onclick={backupNow} disabled={backingUp || !backupTarget}>
					{#if backingUp}<Icon name="progress_activity" size={18} class="animate-spin motion-reduce:animate-none" />{:else}<Icon name="backup" size={18} />{/if}
					{backingUp ? 'Backing up…' : 'Back up now'}
				</button>
				<button class="btn btn-primary" disabled={backupSaving}>{#if backupSaving}<Icon name="progress_activity" size={18} class="animate-spin motion-reduce:animate-none" />{/if}{backupSaving ? 'Saving…' : 'Save'}</button>
			</div>
		{/if}
	</form>
{/if}

<Confirm bind:open={confirmOpen} title="Clean up disk now?" confirmLabel="Clean up" busy={cleaning} onconfirm={cleanNow}>
	{`Old app images beyond the newest ${keep} per app are deleted, so you can no longer roll back to those deployments. Unused build cache is also cleared, so the next build of each app may take longer.`}
</Confirm>
