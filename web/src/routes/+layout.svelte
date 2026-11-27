<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import './layout.css';
	import { onMount } from 'svelte';
	import { afterNavigate, beforeNavigate, goto } from '$app/navigation';
	import { page } from '$app/state';
	import type { LayoutProps } from './$types';
	import { get, post, ApiError, errorMessage, type Node, type User } from '#lib/api.ts';
	import { session } from '#lib/session.svelte.ts';
	import { toasts } from '#lib/toast.svelte.ts';
	import AuthScreen from '#lib/components/AuthScreen.svelte';
	import PageLoader from '#lib/components/PageLoader.svelte';

	let { children }: LayoutProps = $props();

	let view = $state<'loading' | 'setup' | 'login' | 'app' | 'error'>('loading');
	let loadError = $state('');
	let version = $state('');
	let navOpen = $state(false);
	let navigating = $state(false);

	beforeNavigate(({ type }) => {
		if (type !== 'leave') navigating = true;
	});
	afterNavigate(() => {
		navigating = false;
	});

	async function showAuth(next: 'setup' | 'login') {
		session.user = null;
		session.node = null;
		view = next;
		if (page.url.pathname !== '/') await goto('/', { replaceState: true });
	}

	const navItems = [
		{ href: '/', label: 'Apps', icon: 'apps' as const, match: (p: string) => p === '/' || p.startsWith('/apps') },
		{ href: '/blueprints', label: 'Blueprints', icon: 'dashboard_customize' as const },
		{ href: '/backups', label: 'Backups', icon: 'backup' as const },
		{ href: '/notifications', label: 'Notifications', icon: 'notifications' as const },
		{ href: '/maintenance', label: 'Maintenance', icon: 'build' as const },
		{ href: '/settings', label: 'Settings', icon: 'settings' as const, divider: true }
	] as const;
	const active = (item: (typeof navItems)[number]) =>
		'match' in item ? item.match(page.url.pathname) : page.url.pathname.startsWith(item.href);
	const current = $derived(navItems.find(active));

	async function load() {
		try {
			const status = await get<{ setup_required: boolean; version: string }>('/auth/status');
			version = status.version;
			if (status.setup_required) {
				await showAuth('setup');
				return;
			}
			session.user = await get<User>('/auth/me');
			const nodes = await get<Node[]>('/nodes');
			session.node = nodes[0] ?? null;
			view = 'app';
		} catch (e) {
			if (e instanceof ApiError && e.status === 401) await showAuth('login');
			else {
				loadError = errorMessage(e);
				view = 'error';
			}
		}
	}

	async function logout() {
		await post('/auth/logout').catch(() => {});
		await showAuth('login');
	}

	onMount(() => {
		load();
	});
	$effect(() => {
		page.url.pathname;
		navOpen = false;
	});
</script>

<svelte:head><title>forgenerd</title></svelte:head>

{#if view === 'loading'}
	<PageLoader />
{:else if view === 'error'}
	<div class="grid min-h-screen place-items-center px-4 text-center">
		<div>
			<p class="font-medium">The panel could not reach the forgenerd API.</p>
			<p class="mt-1 text-muted">{loadError}</p>
			<button class="btn mt-4" onclick={() => { view = 'loading'; load(); }}>Try again</button>
		</div>
	</div>
{:else if view === 'setup' || view === 'login'}
		<AuthScreen mode={view} {version} ondone={load} />
{:else}
	<div class="shell-app min-h-screen lg:grid lg:grid-cols-[248px_1fr]">
		<aside
			class="shell-sidebar fixed inset-y-0 left-0 z-30 flex w-[248px] flex-col border-r border-line transition-transform duration-200 ease-out lg:sticky lg:top-0 lg:h-screen lg:translate-x-0 {navOpen ? 'translate-x-0' : '-translate-x-full'}"
		>
			<a href="/" class="shell-brand">
				<img class="shell-brand-mark" src="/logo.svg" alt="" width="30" height="30" />
				<span class="shell-brand-name">forgenerd</span>
			</a>
			<nav class="shell-nav" aria-label="Main">
				{#each navItems as item (item.href)}
					{#if 'divider' in item}<div class="shell-nav-divider mt-auto" role="separator"></div>{/if}
					<a
						href={item.href}
						aria-current={active(item) ? 'page' : undefined}
						class="shell-nav-item {active(item) ? 'shell-nav-item-active' : ''}"
					>
						<Icon name={item.icon} size={19} filled={active(item)} />
						<span>{item.label}</span>
					</a>
				{/each}
			</nav>
		</aside>
		{#if navOpen}
			<button class="fixed inset-0 z-20 bg-black/50 lg:hidden" aria-label="Close menu" onclick={() => (navOpen = false)}></button>
		{/if}
		<div class="flex min-w-0 flex-col">
			<div class="route-progress {navigating ? 'route-progress-visible' : ''}" role="progressbar" aria-label="Loading page"></div>
			<header class="shell-header sticky top-0 z-10 flex h-14 items-center gap-3 border-b border-line px-4 sm:px-6">
				<button class="btn btn-ghost btn-sm btn-icon shell-menu-button lg:hidden" aria-label="Open menu" aria-expanded={navOpen} onclick={() => (navOpen = true)}>
					<Icon name="menu" size={22} />
				</button>
				<span class="shell-header-title flex-1">{current?.label ?? ''}</span>
				{#if session.user?.email}
					<span class="shell-header-account" title={session.user.email}>{session.user.email}</span>
				{/if}
				<button class="btn btn-ghost btn-sm" onclick={logout}><Icon name="logout" size={18} /> Log out</button>
			</header>
			<main class="shell-main">
				{@render children()}
			</main>
		</div>
	</div>
{/if}

<div class="pointer-events-none fixed right-4 bottom-4 z-50 flex w-[min(380px,calc(100vw-2rem))] flex-col gap-2" aria-live="polite">
	{#each toasts as t (t.id)}
		<div
			class="pointer-events-auto flex items-start gap-2.5 rounded-md border bg-surface px-4 py-3 text-[13px] shadow-[var(--shadow)] {t.tone === 'bad'
				? 'border-bad/40 text-bad'
				: 'border-line text-text'}"
		>
			<Icon name={t.tone === 'bad' ? 'error' : 'check_circle'} size={18} filled class="shrink-0 {t.tone === 'bad' ? '' : 'text-ok'}" />
			<span>{t.text}</span>
		</div>
	{/each}
</div>
