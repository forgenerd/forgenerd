export interface Toast {
	id: number;
	tone: 'ok' | 'bad';
	text: string;
}

let next = 1;
export const toasts = $state<Toast[]>([]);

export function notify(text: string, tone: Toast['tone'] = 'ok') {
	const id = next++;
	toasts.push({ id, tone, text });
	setTimeout(() => {
		const i = toasts.findIndex((t) => t.id === id);
		if (i >= 0) toasts.splice(i, 1);
	}, tone === 'bad' ? 8000 : 3500);
}
