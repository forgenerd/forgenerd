import type { App, Deployment } from './api.ts';

export interface AppSummary extends App {
	domains: string[];
	last_deployment: Deployment | null;
}

/** The app is serving, but its latest deployment failed (the previous version is live). */
export const deployFailed = (a: AppSummary) =>
	a.last_deployment?.status === 'failed' && !!a.active_deployment_id && a.status !== 'failed';

export const needsAttention = (a: AppSummary) => a.status === 'unhealthy' || a.status === 'failed';

/** Problems first: unhealthy, failed, failed deploy, building, running, stopped. */
function rank(a: AppSummary): number {
	if (a.status === 'unhealthy') return 0;
	if (a.status === 'failed') return 1;
	if (deployFailed(a)) return 2;
	return { building: 3, running: 4, stopped: 5 }[a.status as string] ?? 6;
}

export function sortBoard(apps: AppSummary[]): AppSummary[] {
	return [...apps].sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}

/** `name:port` on forgenerd-internal, once the app has a deployment that answers on it. */
export function internalAddress(a: Pick<App, 'name' | 'container_port' | 'deploy_type' | 'compose_service' | 'active_deployment_id'>): string | null {
	if (!a.active_deployment_id) return null;
	if (a.deploy_type === 'compose' && !a.compose_service) return null;
	return `${a.name}:${a.container_port}`;
}
