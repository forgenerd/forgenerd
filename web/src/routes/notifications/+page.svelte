<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import { get, post, del, errorMessage, type NotificationTarget } from '#lib/api.ts';
	import { notify } from '#lib/toast.svelte.ts';
	import Confirm from '#lib/components/Confirm.svelte';
	import Skeleton from '#lib/components/Skeleton.svelte';

	let targets = $state<NotificationTarget[]>([]);
	let kind = $state<NotificationTarget['kind']>('discord');
	let url = $state('');
	let botToken = $state('');
	let chatId = $state('');
	let error = $state('');
	let busy = $state(false);
	let loaded = $state(false);
	let loadError = $state('');

	const refresh = async () => {
		try {
			targets = await get<NotificationTarget[]>('/notifications');
			loadError = '';
		} catch (e) {
			loadError = errorMessage(e);
		} finally {
			loaded = true;
		}
	};
	onMount(refresh);

	const kinds = { discord: 'Discord webhook', slack: 'Slack incoming webhook', telegram: 'Telegram bot', generic: 'Generic JSON webhook' };

	async function add(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		error = '';
		try {
			await post('/notifications', kind === 'telegram' ? { kind, bot_token: botToken.trim(), chat_id: chatId.trim() } : { kind, url: url.trim() });
			url = '';
			botToken = '';
			chatId = '';
			notify('Notification target added.');
			await refresh();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
	async function test(t: NotificationTarget) {
		try {
			await post(`/notifications/${t.id}/test`);
			notify(`Test message sent to ${t.host}.`);
		} catch (e) {
			notify(errorMessage(e), 'bad');
		}
	}
	let pending = $state<NotificationTarget | null>(null);
	let confirmOpen = $state(false);
	function askRemove(t: NotificationTarget) {
		pending = t;
		confirmOpen = true;
	}
	async function confirmed() {
		if (pending) await remove(pending);
		confirmOpen = false;
	}
	async function remove(t: NotificationTarget) {
		await del(`/notifications/${t.id}`).catch((e) => notify(errorMessage(e), 'bad'));
		await refresh();
	}
</script>

<h1 class="page-title">Notifications</h1>

<form class="panel mt-6 grid gap-4 p-5 sm:grid-cols-[14rem_1fr_auto] sm:items-end" onsubmit={add}>
	<label><span class="label">Type</span>
		<select class="input" bind:value={kind}>{#each Object.entries(kinds) as [k, label] (k)}<option value={k}>{label}</option>{/each}</select>
	</label>
	{#if kind === 'telegram'}
        <div class="grid gap-3 md:grid-cols-2">
            <label><span class="label">Bot token</span><input class="input mono" type="password" bind:value={botToken} placeholder="From @BotFather" required autocomplete="off" spellcheck="false" /></label>
            <label><span class="label">Chat ID or @channel</span><input class="input mono" bind:value={chatId} placeholder="@channel" required autocomplete="off" spellcheck="false" /></label>
        </div>
    {:else}
    <label><span class="label">Webhook URL</span><input class="input mono" bind:value={url} required placeholder="https://discord.com/api/webhooks/…" autocomplete="off" spellcheck="false" /></label>
	{/if}
	<button class="btn btn-primary" disabled={busy}><Icon name={busy ? 'progress_activity' : 'add'} size={18} class={busy ? 'animate-spin motion-reduce:animate-none' : ''} /> {busy ? 'Adding…' : 'Add'}</button>
	{#if error}<p role="alert" class="text-[13px] text-bad sm:col-span-3">{error}</p>{/if}
</form>

{#if !loaded}
	<section class="panel mt-4 overflow-hidden" aria-busy="true" aria-label="Loading notification targets">
		<div class="grid grid-cols-[8rem_1fr_8rem] gap-4 border-b border-line px-5 py-3"><Skeleton class="h-3 w-12" /><Skeleton class="h-3 w-24" /><Skeleton class="h-3 w-12" /></div>
		{#each Array(3) as _, i (i)}
			<div class="grid grid-cols-[8rem_1fr_8rem] items-center gap-4 border-b border-line px-5 py-4 last:border-b-0"><Skeleton class="h-4 w-28" /><Skeleton class="h-3 w-48" /><Skeleton class="ml-auto h-8 w-24" /></div>
		{/each}
	</section>
{:else if loadError}
	<p class="mt-4 rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad" role="alert">Could not load notification targets: {loadError}</p>
{:else}
	<section class="panel mt-4">
		<table class="table">
			<thead><tr><th>Type</th><th>Destination</th><th class="w-0"></th></tr></thead>
			<tbody>
				{#each targets as t (t.id)}
					<tr>
						<td>{kinds[t.kind]}</td>
						<td class="mono text-xs">{t.host}</td>
						<td><div class="flex justify-end gap-1">
							<button class="btn btn-sm" onclick={() => test(t)}><Icon name="send" size={16} /> Send test</button>
							<button class="btn btn-ghost btn-sm btn-icon text-bad" aria-label="Remove {t.host}" onclick={() => askRemove(t)}><Icon name="delete" size={17} /></button>
						</div></td>
					</tr>
				{:else}<tr><td colspan="3" class="py-8 text-center text-muted">No notification targets.</td></tr>{/each}
			</tbody>
		</table>
	</section>
{/if}

<Confirm bind:open={confirmOpen} title="Remove notification target?" confirmLabel="Remove" danger onconfirm={confirmed}>
	{#if pending}{`Alerts will no longer be sent to ${pending.host}.`}{/if}
</Confirm>
