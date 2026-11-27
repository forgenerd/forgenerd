<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { untrack } from 'svelte';
	import { get, patch, errorMessage, type AppDetail, type CertStatus } from '#lib/api.ts';
	import { parseUtc } from '#lib/format.ts';
	import { notify } from '#lib/toast.svelte.ts';
	import { internalAddress } from '#lib/types-board.ts';

	let { app: live, onchange }: { app: AppDetail; onchange: () => void } = $props();
	// The form edits a snapshot; the parent remounts this component when domains change.
	const app = untrack(() => live);

	let rows = $state(app.domains.map((d) => ({ hostname: d.hostname, force_https: d.force_https })));
	let fresh = $state('');
	let busy = $state(false);
	let toggling = $state(false);
	const dirty = $derived(
		JSON.stringify(rows) !== JSON.stringify(app.domains.map((d) => ({ hostname: d.hostname, force_https: d.force_https })))
	);
	/** Hostnames the proxy serves right now: none while the app is not exposed. */
	const served = app.exposed ? [...(app.default_domain ? [app.default_domain] : []), ...app.domains.map((d) => d.hostname)] : [];
	const internal = internalAddress(app);

	// Certificate state per served hostname, read from what the proxy actually serves.
	let certs = $state<Record<string, CertStatus>>({});
	let checking = $state(false);
	async function check() {
		if (served.length === 0) return;
		checking = true;
		try {
			const list = await get<CertStatus[]>(`/apps/${app.id}/certificates`);
			certs = Object.fromEntries(list.map((c) => [c.hostname, c]));
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			checking = false;
		}
	}
	$effect(() => {
		check();
	});

	function expiry(c: CertStatus): string {
		const d = parseUtc(c.not_after);
		if (!d) return '';
		const ms = d.getTime() - Date.now();
		if (ms <= 0) return 'expired';
		const hours = Math.floor(ms / 3_600_000);
		if (hours < 48) return hours < 1 ? 'expires within the hour' : `expires in ${hours} h`;
		return `expires in ${Math.floor(hours / 24)} days`;
	}

	/** Why a hostname has no certificate: an undeployed app has no route yet. */
	function pendingReason(c: CertStatus): string {
		if (!app.active_deployment_id) return 'the app has not been deployed yet, so nothing is routed to it';
		return c.detail ?? 'waiting for the certificate authority';
	}

	async function setExposed(exposed: boolean) {
		toggling = true;
		try {
			await patch(`/apps/${app.id}`, { exposed });
			notify(exposed ? 'App exposed. Routes updated.' : 'App is now internal only. Public routes removed.');
			onchange();
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			toggling = false;
		}
	}

	function add(e: SubmitEvent) {
		e.preventDefault();
		const h = fresh.trim().toLowerCase();
		if (h && h !== app.default_domain && !rows.some((r) => r.hostname === h)) rows.push({ hostname: h, force_https: true });
		fresh = '';
	}

	async function save() {
		busy = true;
		try {
			await patch(`/apps/${app.id}`, { domains: rows });
			notify('Domains saved. Routes updated.');
			onchange();
		} catch (e) {
			notify(errorMessage(e), 'bad');
		} finally {
			busy = false;
		}
	}
</script>

{#snippet cert(hostname: string)}
	{#if certs[hostname]}
		{@const c = certs[hostname]}
		<p class="mt-0.5 flex items-start gap-1.5 text-xs {c.state === 'issued' ? 'text-ok' : c.state === 'local_ca' ? 'text-muted' : c.state === 'pending' ? 'text-warn' : 'text-bad'}">
			{#if c.state === 'issued' || c.state === 'local_ca'}<Icon name="verified_user" size={16} class="mt-px shrink-0" />{:else if c.state === 'pending'}<Icon name="gpp_maybe" size={16} class="mt-px shrink-0" />{:else}<Icon name="gpp_bad" size={16} class="mt-px shrink-0" />{/if}
			<span>
				{#if c.state === 'issued'}Certificate by {c.issuer} · {expiry(c)}
				{:else if c.state === 'local_ca'}Local certificate (not publicly trusted; renewed automatically) · {expiry(c)}
				{:else if c.state === 'expired'}Certificate by {c.issuer} has expired
				{:else if c.state === 'pending'}No certificate yet: {pendingReason(c)}
				{:else}Could not check: {c.detail}{/if}
			</span>
		</p>
	{:else if checking && served.includes(hostname)}
		<p class="mt-0.5 text-xs text-muted">Checking certificate…</p>
	{/if}
{/snippet}

<section class="panel w-full max-w-none">
	<div class="panel-head">
		<h2>Public access</h2>
		<label class="flex cursor-pointer items-center gap-2 text-[13px] font-medium">
			<input type="checkbox" class="size-4" checked={app.exposed} disabled={toggling} onchange={(e) => setExposed(e.currentTarget.checked)} />
			Public
		</label>
	</div>

	<div class="flex items-center justify-between gap-3 border-b border-line px-5 pt-4 pb-2">
		<h3 class="label mb-0">Domains</h3>
		{#if served.length > 0}
			<button class="btn btn-ghost btn-sm" disabled={checking} onclick={check}>
				<Icon name="refresh" size={17} class={checking ? 'animate-spin motion-reduce:animate-none' : ''} /> Check certificates
			</button>
		{/if}
	</div>
	<ul class={app.exposed ? '' : 'opacity-60'}>
		<li class="border-b border-line px-5 py-3">
			{#if app.default_domain}
				<div class="flex flex-wrap items-center gap-2">
					<a href="https://{app.default_domain}" target="_blank" rel="noreferrer" class="flex min-w-0 items-center gap-1.5 font-medium hover:underline">
						<span class="truncate">{app.default_domain}</span><Icon name="open_in_new" size={16} class="shrink-0 text-muted" />
					</a>
					<span class="rounded-md border border-line px-1.5 text-xs text-muted">default</span>
				</div>
				{#if app.exposed}{@render cert(app.default_domain)}{:else}<p class="mt-0.5 text-xs text-muted">Private</p>{/if}
			{:else}
				<p class="text-[13px] text-muted">
					No default domain · <a class="link" href="/settings">Set root domain</a>
				</p>
			{/if}
		</li>
		{#each rows as r, i (r.hostname)}
			<li class="flex flex-wrap items-center gap-3 border-b border-line px-5 py-3">
				<div class="w-full min-w-0 sm:w-auto sm:flex-1">
					<a href="https://{r.hostname}" target="_blank" rel="noreferrer" class="flex items-center gap-1.5 font-medium hover:underline">
						<span class="truncate">{r.hostname}</span><Icon name="open_in_new" size={16} class="shrink-0 text-muted" />
					</a>
					{#if !app.domains.some((d) => d.hostname === r.hostname)}
						<p class="mt-0.5 text-xs text-muted">Not saved yet</p>
					{:else if app.exposed}{@render cert(r.hostname)}{/if}
				</div>
				<label class="flex flex-1 items-center gap-2 text-[13px] text-muted sm:flex-none">
					<input type="checkbox" bind:checked={r.force_https} /> Force HTTPS
				</label>
				<button class="btn btn-ghost btn-sm btn-icon" aria-label="Remove {r.hostname}" onclick={() => rows.splice(i, 1)}><Icon name="close" size={18} /></button>
			</li>
		{/each}
	</ul>
	<form class="flex gap-2 p-5" onsubmit={add}>
		<input class="input" placeholder="www.example.com" bind:value={fresh} aria-label="Custom domain" spellcheck="false" />
		<button class="btn"><Icon name="add" size={18} /> Add domain</button>
	</form>
	<div class="flex justify-end gap-2 border-t border-line px-5 py-3">
		{#if dirty}<button class="btn" onclick={() => (rows = app.domains.map((d) => ({ hostname: d.hostname, force_https: d.force_https })))}>Discard</button>{/if}
		<button class="btn btn-primary" disabled={!dirty || busy} onclick={save}>{#if busy}<Icon name="progress_activity" size={18} class="animate-spin motion-reduce:animate-none" />{/if}{busy ? 'Saving…' : 'Save'}</button>
	</div>
</section>
