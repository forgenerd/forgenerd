<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import type { IconName } from '#lib/icons.ts';

	let { status, size = 'md' }: { status: string; size?: 'sm' | 'md' } = $props();

	const map: Record<string, { label: string; tone: string; icon: IconName; spin?: boolean }> = {
		running: { label: 'Running', tone: 'ok', icon: 'check_circle' },
		healthy: { label: 'Healthy', tone: 'ok', icon: 'check_circle' },
		succeeded: { label: 'Succeeded', tone: 'ok', icon: 'check_circle' },
		online: { label: 'Online', tone: 'ok', icon: 'check_circle' },
		building: { label: 'Building', tone: 'warn', icon: 'progress_activity', spin: true },
		restarting: { label: 'Restarting', tone: 'warn', icon: 'refresh', spin: true },
		queued: { label: 'Queued', tone: 'muted', icon: 'schedule' },
		unhealthy: { label: 'Unhealthy', tone: 'bad', icon: 'error' },
		failed: { label: 'Failed', tone: 'bad', icon: 'cancel' },
		offline: { label: 'Offline', tone: 'bad', icon: 'cancel' },
		stopped: { label: 'Stopped', tone: 'muted', icon: 'pause_circle' },
		rolled_back: { label: 'Rolled back', tone: 'muted', icon: 'undo' },
		running_job: { label: 'Running', tone: 'warn', icon: 'progress_activity', spin: true }
	};
	const s = $derived(map[status] ?? { label: status, tone: 'muted', icon: 'schedule' });
</script>

<span class="status-pill status-{s.tone} {size === 'sm' ? 'status-pill-sm' : ''}">
	<Icon name={s.icon} size={size === 'sm' ? 14 : 15} filled={!s.spin} class={s.spin ? 'animate-spin motion-reduce:animate-none' : ''} />
	{s.label}
</span>
