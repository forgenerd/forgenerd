/** Timestamps from the API are UTC without a zone suffix. */
export function parseUtc(ts: string | null | undefined): Date | null {
	if (!ts) return null;
	const iso = ts.includes('T') ? ts : ts.replace(' ', 'T');
	return new Date(/[zZ]|[+-]\d\d:?\d\d$/.test(iso) ? iso : `${iso}Z`);
}

export function ago(ts: string | null | undefined, now = Date.now()): string {
	const d = parseUtc(ts);
	if (!d) return '-';
	const s = Math.max(0, Math.round((now - d.getTime()) / 1000));
	if (s < 45) return 'just now';
	const m = Math.round(s / 60);
	if (m < 60) return `${m} min ago`;
	const h = Math.round(m / 60);
	if (h < 24) return `${h}h ago`;
	const days = Math.round(h / 24);
	if (days < 30) return `${days}d ago`;
	return d.toLocaleDateString();
}

export function dateTime(ts: string | null | undefined): string {
	const d = parseUtc(ts);
	return d ? d.toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' }) : '-';
}

export function duration(start: string | null | undefined, end: string | null | undefined): string {
	const a = parseUtc(start);
	const b = parseUtc(end);
	if (!a || !b) return '-';
	const s = Math.max(0, Math.round((b.getTime() - a.getTime()) / 1000));
	return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
}

export function bytes(n: number | null | undefined): string {
	if (n == null) return '-';
	const units = ['B', 'KB', 'MB', 'GB', 'TB'];
	let v = n;
	let i = 0;
	while (v >= 1024 && i < units.length - 1) {
		v /= 1024;
		i++;
	}
	return `${v < 10 && i > 0 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

export function shortSha(sha: string | null | undefined): string {
	return sha ? sha.slice(0, 7) : '';
}

/** Short, stable deployment number shown in the UI (last 6 hex chars of the id). */
export function shortId(id: string): string {
	return id.replace(/-/g, '').slice(-6);
}

/** "in 14 h" style countdown to a future UTC timestamp. */
export function until(ts: string | null | undefined, now = Date.now()): string {
	const d = parseUtc(ts);
	if (!d) return '-';
	const s = Math.max(0, Math.round((d.getTime() - now) / 1000));
	if (s < 60) return 'in under a minute';
	const m = Math.round(s / 60);
	if (m < 60) return `in ${m} min`;
	const h = Math.round(m / 60);
	if (h < 48) return `in ${h} h`;
	return `in ${Math.round(h / 24)} days`;
}

const DAYS = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
const pad = (n: string) => n.padStart(2, '0');
const num = (v: string) => /^\d+$/.test(v);

/** Plain words for the common 5-field cron shapes; null when the shape is unusual. */
export function describeCron(expr: string): string | null {
	const f = expr.trim().split(/\s+/);
	if (f.length !== 5) return null;
	const [min, hour, dom, mon, dow] = f;
	if (min === '*' && hour === '*' && dom === '*' && mon === '*' && dow === '*') return 'Every minute';
	const every = /^\*\/(\d+)$/.exec(min);
	if (every && hour === '*' && dom === '*' && mon === '*' && dow === '*') return `Every ${every[1]} minutes`;
	if (num(min) && hour === '*' && dom === '*' && mon === '*' && dow === '*') return `Hourly at :${pad(min)}`;
	if (num(min) && num(hour) && mon === '*') {
		const at = `${pad(hour)}:${pad(min)} UTC`;
		if (dom === '*' && dow === '*') return `Daily at ${at}`;
		if (dom === '*' && num(dow) && Number(dow) <= 7) return `${DAYS[Number(dow) % 7]}s at ${at}`;
		if (dom === '*' && dow === '1-5') return `Weekdays at ${at}`;
		if (num(dom) && dow === '*') return `Monthly on day ${dom} at ${at}`;
	}
	return null;
}
