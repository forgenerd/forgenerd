<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import { get, post, patch, del, errorMessage, type ApiToken, type Settings } from '#lib/api.ts';
	import { ago } from '#lib/format.ts';
	import { notify } from '#lib/toast.svelte.ts';
	import { session } from '#lib/session.svelte.ts';
	import Copy from '#lib/components/Copy.svelte';
	import Confirm from '#lib/components/Confirm.svelte';
	import Skeleton from '#lib/components/Skeleton.svelte';

	let settings = $state<Settings>({ root_domain: null, panel_domain: null, acme_email: null });
	let root = $state('');
	let tokens = $state<ApiToken[]>([]);
	let tokenName = $state('');
	let created = $state<{ name: string; token: string } | null>(null);
	let busy = $state(false);
	let error = $state('');
	let loaded = $state(false);
	let loadError = $state('');

	onMount(async () => {
		try {
			const [nextSettings, nextTokens] = await Promise.all([get<Settings>('/settings'), get<ApiToken[]>('/auth/tokens')]);
			settings = nextSettings;
			root = settings.root_domain ?? '';
			tokens = nextTokens;
		} catch (e) {
			loadError = errorMessage(e);
		} finally {
			loaded = true;
		}
	});

	async function save(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		error = '';
		try {
			settings = await patch<Settings>('/settings', { root_domain: root.trim() || null });
			notify('Settings saved. Proxy updated.');
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function createToken(e: SubmitEvent) {
		e.preventDefault();
		try {
			const t = await post<ApiToken & { token: string }>('/auth/tokens', { name: tokenName.trim() });
			created = { name: t.name, token: t.token };
			tokenName = '';
			tokens = await get<ApiToken[]>('/auth/tokens');
		} catch (err) {
			notify(errorMessage(err), 'bad');
		}
	}
	let pending = $state<ApiToken | null>(null);
	let confirmOpen = $state(false);
	function askRevoke(t: ApiToken) {
		pending = t;
		confirmOpen = true;
	}
	async function confirmed() {
		if (pending) await revoke(pending);
		confirmOpen = false;
	}
	// Password change
	let currentPassword = $state('');
	let newPassword = $state('');
	let confirmPassword = $state('');
	let passwordBusy = $state(false);
	let passwordError = $state('');
	async function changePassword(e: SubmitEvent) {
		e.preventDefault();
		passwordError = '';
		if (newPassword !== confirmPassword) {
			passwordError = 'The new passwords do not match.';
			return;
		}
		passwordBusy = true;
		try {
			await post('/auth/password', { current_password: currentPassword, new_password: newPassword });
			currentPassword = newPassword = confirmPassword = '';
			notify('Password changed. Other sessions were signed out.');
		} catch (err) {
			passwordError = errorMessage(err);
		} finally {
			passwordBusy = false;
		}
	}

	async function revoke(t: ApiToken) {
		await del(`/auth/tokens/${t.id}`).catch((e) => notify(errorMessage(e), 'bad'));
		tokens = await get<ApiToken[]>('/auth/tokens');
		notify(`Revoked ${t.name}.`);
	}
</script>

<h1 class="page-title">Settings</h1>

{#if !loaded}
	<div class="mt-6 grid gap-6" aria-busy="true" aria-label="Loading settings">
		<section class="panel grid gap-4 p-5">
			<div class="grid gap-2"><Skeleton class="h-4 w-44" /><Skeleton class="h-3 w-96 max-w-full" /></div>
			<div class="grid gap-2"><Skeleton class="h-3 w-24" /><Skeleton class="h-9 w-full" /><Skeleton class="h-3 w-3/4" /></div>
		</section>
		<section class="panel overflow-hidden">
			<div class="grid gap-2 border-b border-line p-5"><Skeleton class="h-4 w-24" /><Skeleton class="h-3 w-80 max-w-full" /></div>
			{#each Array(3) as _, i (i)}<div class="flex items-center justify-between border-b border-line px-5 py-4 last:border-b-0"><Skeleton class="h-4 w-32" /><Skeleton class="h-3 w-24" /></div>{/each}
		</section>
	</div>
{:else if loadError}
	<p class="mt-6 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad" role="alert">Could not load settings: {loadError}</p>
{:else}
<form class="panel mt-6" onsubmit={save}>
	<div class="panel-head"><h2>Domain</h2></div>
	<div class="grid gap-4 p-5">
		<label>
			<span class="label">Root domain</span>
			<input class="input max-w-md" bind:value={root} placeholder="example.com" spellcheck="false" />
		</label>
		{#if root.trim()}
			<dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-[13px]">
				<dt class="text-muted">Panel</dt><dd><code>https://hero.{root.trim()}</code></dd>
				<dt class="text-muted">Apps</dt><dd><code>&lt;app&gt;.{root.trim()}</code></dd>
				<dt class="text-muted">DNS</dt><dd><code>*.{root.trim()}</code> → A record to this server</dd>
			</dl>
		{/if}
		{#if error}<p role="alert" class="text-[13px] text-bad">{error}</p>{/if}
	</div>
	<div class="flex justify-end border-t border-line px-5 py-3"><button class="btn btn-primary" disabled={busy}>{#if busy}<Icon name="progress_activity" size={18} class="animate-spin motion-reduce:animate-none" />{/if}{busy ? 'Saving…' : 'Save'}</button></div>
</form>

<form class="panel mt-6" onsubmit={changePassword}>
	<div class="panel-head"><h2>Password</h2></div>
	<div class="grid gap-4 p-5 sm:grid-cols-3">
		<input type="text" class="hidden" autocomplete="username" value={session.user?.email ?? ''} readonly aria-hidden="true" tabindex="-1" />
		<label>
			<span class="label">Current password</span>
			<input class="input" type="password" bind:value={currentPassword} required autocomplete="current-password" />
		</label>
		<label>
			<span class="label">New password</span>
			<input class="input" type="password" bind:value={newPassword} required minlength="12" autocomplete="new-password" />
			<span class="mt-1 block text-xs text-muted">At least 12 characters.</span>
		</label>
		<label>
			<span class="label">Repeat new password</span>
			<input class="input" type="password" bind:value={confirmPassword} required minlength="12" autocomplete="new-password" />
		</label>
		{#if passwordError}<p role="alert" class="text-[13px] text-bad sm:col-span-3">{passwordError}</p>{/if}
	</div>
	<div class="flex items-center justify-between gap-3 border-t border-line px-5 py-3">
		<p class="text-xs text-muted">Signs out every other browser. API tokens keep working.</p>
		<button class="btn btn-primary" disabled={passwordBusy}>{#if passwordBusy}<Icon name="progress_activity" size={18} class="animate-spin motion-reduce:animate-none" />{/if}{passwordBusy ? 'Changing…' : 'Change password'}</button>
	</div>
</form>

<section class="panel mt-6">
	<div class="panel-head"><h2>API tokens</h2></div>
	{#if created}
		<div class="border-b border-line bg-warn-soft p-5">
			<p class="text-[13px] font-medium">Copy the token for “{created.name}” now. It will not be shown again.</p>
			<div class="mt-2 flex items-center gap-1">
				<code class="input flex items-center overflow-x-auto bg-surface whitespace-nowrap">{created.token}</code>
				<Copy value={created.token} label="Copy token" />
			</div>
			<button class="btn btn-sm mt-3" onclick={() => (created = null)}>Done</button>
		</div>
	{/if}
	<ul class="divide-y divide-line">
		{#each tokens as t (t.id)}
			<li class="flex items-center gap-3 px-5 py-3">
				<span class="flex-1 font-medium">{t.name}</span>
				<span class="text-xs text-muted">created {ago(t.created_at)}</span>
				<button class="btn btn-ghost btn-sm text-bad" onclick={() => askRevoke(t)}><Icon name="delete" size={17} /> Revoke</button>
			</li>
		{:else}<li class="px-5 py-5 text-center text-[13px] text-muted">No API tokens.</li>{/each}
	</ul>
	<form class="flex gap-2 border-t border-line p-5" onsubmit={createToken}>
		<input class="input max-w-xs" bind:value={tokenName} required maxlength="100" placeholder="github-actions" aria-label="Token name" />
		<button class="btn"><Icon name="add" size={18} /> Create token</button>
	</form>
</section>
{/if}

<Confirm bind:open={confirmOpen} title="Revoke API token?" confirmLabel="Revoke" danger onconfirm={confirmed}>
	{#if pending}{`${pending.name} stops working immediately. Scripts using it will get 401 errors.`}{/if}
</Confirm>
