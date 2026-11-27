<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onMount, untrack } from 'svelte';
	import { get, patch, queueRedeploy, errorMessage, type AppDetail, type ComposeService } from '#lib/api.ts';
	import { notify } from '#lib/toast.svelte.ts';

	let { app: live, onsaved }: { app: AppDetail; onsaved: () => void | Promise<void> } = $props();
	// The form edits a snapshot; the parent remounts this component after each save.
	const app = untrack(() => live);

	type EnvRow = { key: string; value: string; secret: boolean; stored: boolean; storedSecret: boolean };
	const initialEnv = (): EnvRow[] =>
		app.env.map((e) => ({ key: e.key, value: e.value ?? '', secret: e.is_secret, stored: true, storedSecret: e.is_secret }));

	let port = $state(app.container_port);
	let health = $state(app.healthcheck_path ?? '/');
	// Empty means the default (no limit, or the 30 second timeout).
	const field = (v: number | null) => (v === null ? '' : String(v));
	let healthTimeout = $state(field(app.health_timeout_secs));
	let memory = $state(field(app.memory_limit_mb));
	let cpus = $state(field(app.cpu_limit));
	// Number inputs bind numbers, or null when cleared.
	const parsed = (v: string | number | null | undefined) => (v === null || v === undefined || String(v).trim() === '' ? null : Number(v));

	// Compose apps: limits live in the compose file, one row per service.
	type ServiceLimits = { name: string; memory: string; cpus: string; storedMemory: number | null; storedCpus: number | null };
	let services = $state<ServiceLimits[] | null>(null);
	let servicesError = $state('');
	const toLimits = (list: ComposeService[]): ServiceLimits[] =>
		list.map((s) => ({
			name: s.name,
			memory: field(s.memory_limit_mb),
			cpus: field(s.cpu_limit),
			storedMemory: s.memory_limit_mb,
			storedCpus: s.cpu_limit
		}));
	onMount(async () => {
		if (app.deploy_type !== 'compose') return;
		try {
			services = toLimits(await get<ComposeService[]>(`/apps/${app.id}/services`));
		} catch (err) {
			servicesError = errorMessage(err);
		}
	});
	function serviceChanges() {
		return (services ?? [])
			.map((s) => {
				const change: Record<string, number | null> = {};
				if (parsed(s.memory) !== s.storedMemory) change.memory_limit_mb = parsed(s.memory);
				if (parsed(s.cpus) !== s.storedCpus) change.cpu_limit = parsed(s.cpus);
				return { name: s.name, change };
			})
			.filter((s) => Object.keys(s.change).length > 0);
	}
	let env = $state<EnvRow[]>(initialEnv());
	let volumes = $state(app.volumes.map((v) => ({ name: v.name, mount_path: v.mount_path })));
	let busy = $state(false);
	let error = $state('');

	function addEnv() {
		env.push({ key: '', value: '', secret: true, stored: false, storedSecret: false });
	}
	function removeEnv(i: number) {
		env.splice(i, 1);
	}
	function cancel() {
		port = app.container_port;
		health = app.healthcheck_path ?? '/';
		healthTimeout = field(app.health_timeout_secs);
		memory = field(app.memory_limit_mb);
		cpus = field(app.cpu_limit);
		env = initialEnv();
		volumes = app.volumes.map((v) => ({ name: v.name, mount_path: v.mount_path }));
		services = services?.map((s) => ({ ...s, memory: field(s.storedMemory), cpus: field(s.storedCpus) })) ?? null;
		error = '';
	}

	function environmentPatch(): Record<string, { value: string; is_secret: boolean } | null> {
		const rows = new Map<string, EnvRow>();
		for (const row of env) {
			const key = row.key.trim();
			if (key) rows.set(key, row);
		}

		const envPatch: Record<string, { value: string; is_secret: boolean } | null> = {};
		for (const original of app.env) {
			const row = rows.get(original.key);
			if (!row) {
				envPatch[original.key] = null;
				continue;
			}
			// A stored secret with an empty field is left unchanged.
			if (row.storedSecret && row.value === '') continue;
			if (original.value !== null && original.value === row.value && original.is_secret === row.secret) continue;
			envPatch[original.key] = { value: row.value, is_secret: row.secret };
		}
		for (const [key, row] of rows) {
			if (!app.env.some((original) => original.key === key)) {
				envPatch[key] = { value: row.value, is_secret: row.secret };
			}
		}
		return envPatch;
	}

	async function save(e: SubmitEvent) {
		e.preventDefault();
		error = '';
		const body: Record<string, unknown> = {};
		if (port !== app.container_port) body.container_port = Number(port);
		if (health !== (app.healthcheck_path ?? '/')) body.healthcheck_path = health;
		if (parsed(healthTimeout) !== app.health_timeout_secs) body.health_timeout_secs = parsed(healthTimeout);
		if (app.deploy_type !== 'compose') {
			if (parsed(memory) !== app.memory_limit_mb) body.memory_limit_mb = parsed(memory);
			if (parsed(cpus) !== app.cpu_limit) body.cpu_limit = parsed(cpus);
		}
		const envPatch = environmentPatch();
		if (Object.keys(envPatch).length) body.env = envPatch;
		const nextVolumes = volumes
			.map((v) => ({ name: v.name.trim(), mount_path: v.mount_path.trim() }))
			.filter((v) => v.name && v.mount_path);
		const initialVolumes = app.volumes.map((v) => ({ name: v.name, mount_path: v.mount_path }));
		if (app.deploy_type !== 'compose' && JSON.stringify(nextVolumes) !== JSON.stringify(initialVolumes)) {
			body.volumes = nextVolumes;
		}
		const limitChanges = serviceChanges();
		if (Object.keys(body).length === 0 && limitChanges.length === 0) {
			cancel();
			return;
		}
		busy = true;
		try {
			let redeploy = false;
			for (const { name, change } of limitChanges) {
				const r = await patch<{ redeploy_required: boolean }>(`/apps/${app.id}/services/${encodeURIComponent(name)}`, change);
				redeploy ||= r.redeploy_required;
			}
			if (Object.keys(body).length) {
				const r = await patch<AppDetail>(`/apps/${app.id}`, body);
				redeploy ||= !!r.redeploy_required;
			}
			try {
				await queueRedeploy(app.id, redeploy);
			} catch (err) {
				error = `Saved, but restart could not be queued: ${errorMessage(err)}`;
				return;
			}
			notify(redeploy ? 'Saved. Restart queued.' : 'Saved.');
			await onsaved();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

{#if app.deploy_type === 'draft'}
	<section class="panel p-6">
		<h2 class="text-[15px] font-semibold">Not configured</h2>
		<a class="btn btn-primary mt-4" href="?tab=deployments">Set deployment method</a>
	</section>
{:else}
	<form class="grid w-full max-w-none gap-6" onsubmit={save}>
		<section class="panel">
			<div class="panel-head">
				<h2>Environment variables</h2>
				<button type="button" class="btn btn-sm" onclick={addEnv}><Icon name="add" size={17} /> Add</button>
			</div>
			<div class="divide-y divide-line">
				{#each env as row, i (i)}
					<div class="grid items-center gap-2 px-5 py-2.5 sm:grid-cols-[minmax(0,14rem)_minmax(0,1fr)_auto_auto]">
						<input class="input mono" bind:value={row.key} placeholder="NAME" readonly={row.stored} aria-label="Variable name" spellcheck="false" />
						<input
							class="input mono"
							type={row.secret ? 'password' : 'text'}
							bind:value={row.value}
							placeholder={row.stored && row.storedSecret ? 'stored · type to replace' : 'value'}
							aria-label="Value of {row.key || 'new variable'}"
							autocomplete="off"
							spellcheck="false"
						/>
						<label class="flex items-center gap-1.5 text-xs text-muted" title="Secret values are write-only">
							<input type="checkbox" bind:checked={row.secret} disabled={row.stored && row.storedSecret && row.value === ''} />
							<Icon name="lock" size={14} /> Secret
						</label>
						<button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label="Remove {row.key}" onclick={() => removeEnv(i)}><Icon name="close" size={18} /></button>
					</div>
				{:else}
					<p class="px-5 py-6 text-center text-[13px] text-muted">No environment variables.</p>
				{/each}
			</div>
		</section>

		<section class="panel">
			<div class="panel-head"><h2>Runtime</h2></div>
			<div class="grid gap-4 p-5 sm:grid-cols-2">
				<label><span class="label">Container port</span><input class="input" type="number" min="1" max="65535" bind:value={port} /></label>
				<label><span class="label">Health check path</span><input class="input mono" bind:value={health} placeholder="/" /></label>
				<label>
					<span class="label">Health check timeout (seconds)</span>
					<input class="input" type="number" min="5" max="900" bind:value={healthTimeout} placeholder="30" />
					<span class="mt-1 block text-xs text-muted">How long a new version may take to answer before the deploy is rolled back.</span>
				</label>
			</div>
		</section>

		<section class="panel">
			<div class="panel-head"><h2>Resource limits</h2></div>
			{#if app.deploy_type === 'compose'}
				{#if servicesError}
					<p role="alert" class="p-5 text-[13px] text-bad">Could not load the services: {servicesError}</p>
				{:else if !services}
					<p class="p-5 text-[13px] text-muted">Loading services…</p>
				{:else}
					<div class="hidden grid-cols-[minmax(0,1fr)_12rem_12rem] gap-3 border-b border-line px-5 py-2 text-xs text-muted sm:grid">
						<span>Service</span><span>Memory limit (MiB)</span><span>CPU limit (cores)</span>
					</div>
					<div class="divide-y divide-line">
						{#each services as s (s.name)}
							<div class="grid items-center gap-2 px-5 py-2.5 sm:grid-cols-[minmax(0,1fr)_12rem_12rem] sm:gap-3">
								<span class="mono truncate text-[13px]">{s.name}</span>
								<input class="input" type="number" min="16" max="1048576" bind:value={s.memory} placeholder="Unlimited" aria-label="Memory limit of {s.name} in MiB" />
								<input class="input" type="number" min="0.01" max="1024" step="0.01" bind:value={s.cpus} placeholder="Unlimited" aria-label="CPU limit of {s.name} in cores" />
							</div>
						{/each}
					</div>
					<p class="border-t border-line px-5 py-3 text-xs text-muted">Saved into the compose file as <code>mem_limit</code> and <code>cpus</code>. Leave empty for no limit.</p>
				{/if}
			{:else}
				<div class="grid gap-4 p-5 sm:grid-cols-2">
					<label>
						<span class="label">Memory limit (MiB)</span>
						<input class="input" type="number" min="16" max="1048576" bind:value={memory} placeholder="Unlimited" />
						<span class="mt-1 block text-xs text-muted">The app is restarted if it uses more. Leave empty for no limit.</span>
					</label>
					<label>
						<span class="label">CPU limit (cores)</span>
						<input class="input" type="number" min="0.01" max="1024" step="0.01" bind:value={cpus} placeholder="Unlimited" />
						<span class="mt-1 block text-xs text-muted">For example 0.5 for half a core. Leave empty for no limit.</span>
					</label>
				</div>
			{/if}
		</section>

	{#if app.deploy_type !== 'compose'}
		<section class="panel">
			<div class="panel-head">
			<h2>Persistent volumes</h2>
				<button type="button" class="btn btn-sm" onclick={() => volumes.push({ name: '', mount_path: '' })}><Icon name="add" size={17} /> Add</button>
			</div>
			<div class="divide-y divide-line">
				{#each volumes as v, i (i)}
					<div class="grid items-center gap-2 px-5 py-2.5 sm:grid-cols-[minmax(0,14rem)_minmax(0,1fr)_auto]">
						<input class="input mono" bind:value={v.name} placeholder="data" aria-label="Volume name" />
						<input class="input mono" bind:value={v.mount_path} placeholder="/var/lib/data" aria-label="Mount path" />
						<button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label="Remove volume" onclick={() => volumes.splice(i, 1)}><Icon name="close" size={18} /></button>
					</div>
				{:else}
					<p class="px-5 py-6 text-center text-[13px] text-muted">No volumes.</p>
				{/each}
			</div>
		</section>
	{/if}

	{#if error}<p role="alert" class="rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">{error}</p>{/if}
	<div class="flex items-center gap-3">
		<button class="btn btn-primary" disabled={busy}>{#if busy}<Icon name="progress_activity" size={18} class="animate-spin motion-reduce:animate-none" />{/if}{busy ? 'Saving…' : 'Save changes'}</button>
	</div>
	</form>
{/if}
