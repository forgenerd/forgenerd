export class ApiError extends Error {
	constructor(
		public status: number,
		message: string
	) {
		super(message);
	}
}

/** JSON request to the forgenerd API. State-changing calls carry the JSON content type. */
export async function api<T>(method: string, path: string, body?: unknown): Promise<T> {
	const res = await fetch(`/api/v1${path}`, {
		method,
		credentials: 'same-origin',
		headers: method === 'GET' ? {} : { 'Content-Type': 'application/json' },
		body: body === undefined ? undefined : JSON.stringify(body)
	});
	if (!res.ok) {
		const data = await res.json().catch(() => ({}));
		throw new ApiError(res.status, data.error ?? res.statusText);
	}
	if (res.status === 204) return undefined as T;
	const type = res.headers.get('content-type') ?? '';
	return (type.includes('json') ? res.json() : res.text()) as Promise<T>;
}

export const get = <T>(path: string) => api<T>('GET', path);
export const post = <T>(path: string, body: unknown = {}) => api<T>('POST', path, body);
export const patch = <T>(path: string, body: unknown) => api<T>('PATCH', path, body);
export const del = <T = void>(path: string) => api<T>('DELETE', path);

/** Apply saved container configuration to a deployed app. */
export async function queueRedeploy(appId: string, required: boolean): Promise<void> {
	if (required) await post(`/apps/${appId}/deploy`);
}

/** WebSocket URL for an API path on the current origin (ws:// or wss://). */
export function wsUrl(path: string): string {
	const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
	return `${proto}//${location.host}/api/v1${path}`;
}

export function errorMessage(e: unknown): string {
	return e instanceof Error ? e.message : String(e);
}

export type AppStatus = 'running' | 'stopped' | 'building' | 'failed' | 'unhealthy';
export type DeployStatus = 'queued' | 'building' | 'healthy' | 'failed' | 'rolled_back';
export type DeployType = 'draft' | 'image' | 'git' | 'compose';

export interface User {
	id: string;
	email: string;
	role: string;
	created_at: string;
}

export interface Node {
	id: string;
	name: string;
	mode: string;
	public_ip: string | null;
	status: string;
	last_seen_at: string;
	created_at: string;
}

export interface Project {
	id: string;
	name: string;
	created_at: string;
}

export interface App {
	id: string;
	project_id: string;
	node_id: string;
	name: string;
	deploy_type: DeployType;
	image_name: string | null;
	git_repo_url: string | null;
	git_branch: string | null;
	build_method: string | null;
	compose_service: string | null;
	container_port: number;
	healthcheck_path: string | null;
	active_container_id: string | null;
	active_deployment_id: string | null;
	status: AppStatus;
	/** Served publicly: default domain (<name>.<root>) plus custom domains. */
	exposed: boolean;
	/** Hard memory limit in MiB; null = unlimited. Image and git apps only. */
	memory_limit_mb: number | null;
	/** CPU limit in cores; null = unlimited. Image and git apps only. */
	cpu_limit: number | null;
	/** Health check deadline for new deployments; null = 30 seconds. */
	health_timeout_secs: number | null;
	created_at: string;
	updated_at: string;
}

export interface Domain {
	id: string;
	hostname: string;
	force_https: boolean;
	created_at: string;
}

export interface EnvVar {
	key: string;
	value: string | null;
	is_secret: boolean;
}

export interface Volume {
	id: string;
	name: string;
	mount_path: string;
}

export interface Deployment {
	id: string;
	app_id: string;
	commit_sha: string | null;
	image_ref: string | null;
	status: DeployStatus;
	started_at: string;
	finished_at: string | null;
}

export interface AppDetail extends App {
	/** <name>.<root_domain>, when a root domain is set and nothing else claims it. */
	default_domain: string | null;
	domains: Domain[];
	env: EnvVar[];
	volumes: Volume[];
	deployments: Deployment[];
	redeploy_required?: boolean;
}

export interface ComposeService {
	name: string;
	container_name: string;
	image: string | null;
	environment: ComposeEnvironment[];
	volumes: string[];
	ports: string[];
	depends_on: string[];
	/** From mem_limit in the compose file, in MiB; null = unlimited. */
	memory_limit_mb: number | null;
	/** From cpus in the compose file; null = unlimited. */
	cpu_limit: number | null;
	present: boolean;
	running: boolean;
	restarting: boolean;
	restart_count: number;
	health: string | null;
	exposed: boolean;
}

export interface ComposeEnvironment {
	key: string;
	value: string | null;
}

export interface BlueprintParameter {
	key: string;
	default_value: string | null;
	description: string;
	secret: boolean;
	generate: boolean;
}

export interface Blueprint {
	id: string;
	name: string;
	description: string;
	port: number;
	image: string;
	image_tag: string;
	parameters: BlueprintParameter[];
}

export interface S3Target {
	id: string;
	name: string;
	endpoint: string;
	bucket: string;
	region: string;
	last_test_at: string | null;
	last_test_ok: boolean | null;
	last_test_error: string | null;
}

export interface BackupRun {
	id: string;
	status: 'running' | 'succeeded' | 'failed';
	trigger: 'schedule' | 'manual';
	object_key: string | null;
	size_bytes: number | null;
	error: string | null;
	started_at: string;
	finished_at: string | null;
}

export interface ContainerStats {
	id: string;
	name: string;
	/** Percent of one core; can exceed 100 on multi-core hosts. */
	cpu_percent: number;
	memory_bytes: number;
	/** The container's limit, or the host's memory when unlimited. */
	memory_limit_bytes: number;
}

export interface DiskUsage {
	images_bytes: number;
	images_reclaimable_bytes: number;
	containers_bytes: number;
	volumes_bytes: number;
	build_cache_bytes: number;
	filesystem_total_bytes: number;
	filesystem_free_bytes: number;
}

export interface PanelBackupRun {
	finished_at: string;
	ok: boolean;
	object_key: string | null;
	size_bytes: number | null;
	error: string | null;
}

export interface PanelBackupConfig {
	s3_target_id: string | null;
	cron_expr: string | null;
	retention_count: number;
	last_run: PanelBackupRun | null;
}

export interface BackupJob {
	id: string;
	app_id: string;
	s3_target_id: string;
	driver: 'postgres' | 'mysql' | 'volume';
	compose_service: string | null;
	volume_id: string | null;
	db_name: string | null;
	db_user: string | null;
	has_db_password: boolean;
	cron_expr: string;
	retention_count: number;
	enabled: boolean;
	next_run_at: string | null;
	last_run: BackupRun | null;
}

export interface NotificationTarget {
	id: string;
	kind: 'discord' | 'slack' | 'telegram' | 'generic';
	host: string;
	enabled: boolean;
}

export interface ApiToken {
	id: string;
	name: string;
	created_at: string;
}

export interface Settings {
	root_domain: string | null;
	panel_domain: string | null;
	acme_email: string | null;
}

export interface UpdateStatus {
	current: string;
	latest: string;
	update_available: boolean;
	/** GitHub owner/name releases come from (forgenerd/forgenerd unless overridden). */
	repo: string;
	release_url: string;
}

export interface CleanupConfig {
	cron_expr: string | null;
	keep_images: number;
}

export interface CleanupReport {
	removed_images: string[];
	reclaimed_bytes: number;
	errors: string[];
}

export interface CertStatus {
	hostname: string;
	state: 'issued' | 'local_ca' | 'expired' | 'pending' | 'error';
	issuer: string | null;
	not_after: string | null;
	detail: string | null;
}
