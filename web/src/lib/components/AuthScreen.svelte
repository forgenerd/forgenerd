<script lang="ts">
	import { post, errorMessage } from '#lib/api.ts';
	import Icon from '#lib/components/Icon.svelte';

	let { mode, version, ondone }: { mode: 'setup' | 'login'; version: string; ondone: () => void } = $props();

	let setupToken = $state('');
	let email = $state('');
	let password = $state('');
	let error = $state('');
	let busy = $state(false);

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		error = '';
		try {
			if (mode === 'setup') {
				await post('/auth/setup', { setup_token: setupToken.trim(), email, password });
			}
			await post('/auth/login', { email, password });
			password = '';
			ondone();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<main class="grid min-h-screen place-items-center px-4">
	<div class="w-full max-w-sm">
		<p class="mb-6 flex items-center justify-center gap-2.5 text-xl font-bold tracking-tight"><img class="shell-brand-mark" src="/logo.svg" alt="" width="30" height="30" />forgenerd</p>
		<form class="panel flex flex-col gap-4 p-6" onsubmit={submit}>
			<h1 class="text-[17px] font-semibold">{mode === 'setup' ? 'Create admin account' : 'Sign in'}</h1>
			{#if mode === 'setup'}
				<label>
					<span class="label">Setup token</span>
					<input class="input mono" bind:value={setupToken} placeholder="From the server log" required autocomplete="off" spellcheck="false" />
				</label>
			{/if}
			<label>
				<span class="label">Email</span>
				<input class="input" type="email" bind:value={email} required autocomplete="username" />
			</label>
			<label>
				<span class="label">Password</span>
				<input
					class="input"
					type="password"
					bind:value={password}
					required
					minlength={mode === 'setup' ? 12 : undefined}
					autocomplete={mode === 'setup' ? 'new-password' : 'current-password'}
					placeholder={mode === 'setup' ? '12+ characters' : undefined}
				/>
			</label>
			{#if error}
				<p role="alert" class="rounded-md bg-bad-soft px-3 py-2 text-[13px] text-bad">{error}</p>
			{/if}
			<button class="btn btn-primary w-full" disabled={busy}>
				{busy ? 'Working…' : mode === 'setup' ? 'Create account' : 'Sign in'}
			</button>
		</form>
		<p class="mt-3 text-center text-xs text-muted">v{version}</p>
	</div>
</main>
