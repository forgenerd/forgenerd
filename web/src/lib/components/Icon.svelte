<script lang="ts">
	import { icons, type IconName } from '#lib/icons.ts';

	let {
		name,
		size = 20,
		filled = false,
		class: className = '',
		label
	}: {
		name: IconName;
		size?: number;
		/** The filled variant marks a selected or active state (Material convention). */
		filled?: boolean;
		class?: string;
		/** Accessible name; omit for decorative icons next to visible text. */
		label?: string;
	} = $props();
</script>

<svg
	width={size}
	height={size}
	viewBox="0 -960 960 960"
	fill="currentColor"
	class="shrink-0 {className}"
	aria-hidden={label ? undefined : 'true'}
	role={label ? 'img' : undefined}
	aria-label={label}
	focusable="false"><path d={icons[name][filled ? 1 : 0]} /></svg
>
