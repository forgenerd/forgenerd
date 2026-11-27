<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onDestroy } from 'svelte';
	import { wsUrl } from '#lib/api.ts';

	/** `path` is an API WebSocket path, e.g. /apps/<id>/logs. */
	let {
		path,
		height = '28rem',
		fill = false,
		reconnect = false,
		empty = 'Waiting for output…'
	}: { path: string; height?: string; fill?: boolean; reconnect?: boolean; empty?: string } = $props();

	/** With `fill`, size the pane from its own position so the page never scrolls. */
	let fitted = $state<string | null>(null);
	function fit() {
		if (!fill || !box) return;
		const top = box.getBoundingClientRect().top + window.scrollY;
		fitted = `${Math.max(320, window.innerHeight - top - 32)}px`;
	}
	$effect(() => {
		if (!fill) return;
		fit();
		window.addEventListener('resize', fit);
		return () => window.removeEventListener('resize', fit);
	});

	const MAX_LINES = 5000;
	let lines = $state<string[]>([]);
	let conn = $state<'connecting' | 'live' | 'ended' | 'error'>('connecting');
	let filter = $state('');
	let wrap = $state(true);
	let follow = $state(true);
	let box: HTMLDivElement | undefined = $state();
	let socket: WebSocket | null = null;
	let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
	let connectionVersion = 0;

	function stop() {
		connectionVersion += 1;
		if (reconnectTimer !== null) {
			clearTimeout(reconnectTimer);
			reconnectTimer = null;
		}
		socket?.close();
		socket = null;
	}

	function scheduleReconnect(p: string, version: number) {
		if (!reconnect || reconnectTimer !== null) return;
		reconnectTimer = setTimeout(() => {
			reconnectTimer = null;
			if (version === connectionVersion) connect(p);
		}, 1000);
	}

	function connect(p: string) {
		stop();
		lines = [];
		conn = 'connecting';
		const ws = new WebSocket(wsUrl(p));
		socket = ws;
		const version = connectionVersion;
		ws.onopen = () => {
			if (socket === ws && version === connectionVersion) conn = 'live';
		};
		ws.onmessage = (e) => {
			if (socket !== ws || version !== connectionVersion) return;
			lines.push(String(e.data));
			if (lines.length > MAX_LINES) lines.splice(0, lines.length - MAX_LINES);
			if (follow) queueMicrotask(() => box && (box.scrollTop = box.scrollHeight));
		};
		ws.onerror = () => {
			if (socket !== ws || version !== connectionVersion) return;
			conn = 'error';
			ws.close();
		};
		ws.onclose = () => {
			if (socket !== ws || version !== connectionVersion) return;
			socket = null;
			if (reconnect) {
				conn = 'connecting';
				scheduleReconnect(p, version);
			} else if (conn !== 'error') conn = 'ended';
		};
	}

	$effect(() => connect(path));
	onDestroy(stop);

	const shown = $derived.by(() => {
		if (!filter) return lines;
		try {
			const re = new RegExp(filter, 'i');
			return lines.filter((l) => re.test(l));
		} catch {
			const f = filter.toLowerCase();
			return lines.filter((l) => l.toLowerCase().includes(f));
		}
	});

	function onScroll() {
		if (!box) return;
		follow = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
	}

	const connLabel = { connecting: 'Connecting', live: 'Live', ended: 'Stream ended', error: 'Connection failed' };
	type LogSegment = { text: string; className?: string };
	const ansiSgr = /\u001b\[([0-9;]*)m/g;
	const ansiColors: Record<number, string> = {
		30: 'log-ansi-muted',
		31: 'log-ansi-red',
		32: 'log-ansi-green',
		33: 'log-ansi-yellow',
		34: 'log-ansi-blue',
		35: 'log-ansi-magenta',
		36: 'log-ansi-cyan',
		37: 'log-ansi-text',
		90: 'log-ansi-muted',
		91: 'log-ansi-red',
		92: 'log-ansi-green',
		93: 'log-ansi-yellow',
		94: 'log-ansi-blue',
		95: 'log-ansi-magenta',
		96: 'log-ansi-cyan',
		97: 'log-ansi-text'
	};

	function stripAnsi(line: string): string {
		return line.replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, '');
	}

	function ansiClass(code: string): string {
		const codes = code.split(';').map(Number);
		const color = codes.find((value) => value >= 30 && value <= 37) ?? codes.find((value) => value >= 90 && value <= 97);
		const colorClass = ansiColors[color ?? -1] ?? '';
		return [colorClass, codes.includes(1) ? 'log-ansi-bold' : ''].filter(Boolean).join(' ');
	}

	function lineSegments(line: string): LogSegment[] {
		const segments: LogSegment[] = [];
		let start = 0;
		let activeClass = '';
		ansiSgr.lastIndex = 0;
		let match: RegExpExecArray | null;
		while ((match = ansiSgr.exec(line))) {
			if (match.index > start) segments.push({ text: line.slice(start, match.index), className: activeClass || undefined });
			activeClass = match[1] === '0' || match[1] === '' ? '' : ansiClass(match[1]);
			start = ansiSgr.lastIndex;
		}
		if (start < line.length) segments.push({ text: line.slice(start), className: activeClass || undefined });
		if (segments.length === 0) segments.push({ text: '' });

		const timestamp = segments[0].text.match(/^(\d{2}:\d{2}:\d{2})(\s+)/);
		if (timestamp) {
			const first = segments.shift()!;
			const rest = first.text.slice(timestamp[0].length);
			segments.unshift(
				{ text: timestamp[1], className: 'log-timestamp' },
				{ text: timestamp[2], className: first.className },
				...(rest ? [{ text: rest, className: first.className }] : [])
			);
		}
		return segments;
	}

	function lineTone(line: string): string {
		const clean = stripAnsi(line);
		if (/\b(error|failed|failure|fatal|exception)\b/i.test(clean)) return 'log-line-error';
		if (/\b(warn(?:ing)?|warnings found)\b/i.test(clean)) return 'log-line-warning';
		if (/\b(done|healthy|complete|completed|reachable|ready|success|switched)\b/i.test(clean)) return 'log-line-success';
		if (/\b(starting|waiting|draining|unpacking|exporting|naming)\b/i.test(clean)) return 'log-line-info';
		return '';
	}
</script>

<div class="flex flex-col gap-2">
	<div class="flex flex-wrap items-center gap-2">
		<input class="input max-w-sm" placeholder="Filter (text or regex)" bind:value={filter} aria-label="Filter log lines" />
		<span class="flex-1"></span>
		{#if !follow}
			<button class="btn btn-sm" onclick={() => { follow = true; box && (box.scrollTop = box.scrollHeight); }}>
				<Icon name="vertical_align_bottom" size={17} /> Follow
			</button>
		{/if}
		<button class="btn btn-ghost btn-sm" aria-pressed={wrap} onclick={() => (wrap = !wrap)}>
			<Icon name="wrap_text" size={17} /> {wrap ? 'Wrapping' : 'No wrap'}
		</button>
	</div>
	<div
		bind:this={box}
		onscroll={onScroll}
		class="overflow-auto rounded-lg border border-line bg-[var(--code-bg)] p-3 font-mono text-[12.5px] leading-[1.6]"
		style:height={fitted ?? height}
		role="log"
		aria-live="off"
	>
		{#if shown.length === 0}
			<p class="text-muted">{conn === 'error' ? 'Could not connect to the log stream.' : filter ? 'No lines match the filter.' : empty}</p>
		{:else}
			{#each shown as line, i (i)}
				{@const segments = lineSegments(line)}
				<div class="log-line {lineTone(line)} {wrap ? 'whitespace-pre-wrap break-all' : 'whitespace-pre'}">
					{#each segments as segment}<span class={segment.className}>{segment.text}</span>{/each}
				</div>
			{/each}
		{/if}
	</div>
</div>

<style>
	.log-line {
		min-height: 1.6em;
	}
	.log-line-info {
		color: var(--accent-text);
	}
	.log-line-success {
		color: var(--ok);
	}
	.log-line-warning {
		color: var(--warn);
	}
	.log-line-error {
		color: var(--bad);
	}
	.log-timestamp {
		color: var(--muted);
	}
	.log-ansi-bold {
		font-weight: 700;
	}
	.log-ansi-muted {
		color: var(--muted);
	}
	.log-ansi-red {
		color: var(--bad);
	}
	.log-ansi-green {
		color: var(--ok);
	}
	.log-ansi-yellow {
		color: var(--warn);
	}
	.log-ansi-blue,
	.log-ansi-cyan {
		color: var(--accent-text);
	}
	.log-ansi-magenta,
	.log-ansi-text {
		color: var(--text);
	}
</style>
