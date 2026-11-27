CREATE TABLE users (
    id TEXT PRIMARY KEY,
    email TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,          -- Argon2id
    role TEXT NOT NULL DEFAULT 'admin',   -- Prepared for future RBAC
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    token_hash TEXT UNIQUE NOT NULL,      -- SHA-256 of the cookie token; raw token never stored
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at DATETIME NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE api_tokens (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    token_hash TEXT UNIQUE NOT NULL,      -- SHA-256 of a 32-byte random token (lookup by hash)
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,                 -- e.g. 'panel_domain', 'acme_email'
    value TEXT NOT NULL
);

CREATE TABLE nodes (
    id TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    mode TEXT NOT NULL DEFAULT 'local',   -- 'local' | 'agent'
    public_ip TEXT,
    agent_token_hash TEXT,                -- For future remote worker authentication
    status TEXT NOT NULL DEFAULT 'online',-- 'online' | 'offline'
    last_seen_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT OR IGNORE INTO nodes (id, name, mode, status) VALUES ('local', 'Primary Server', 'local', 'online');

CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE apps (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL DEFAULT 'local' REFERENCES nodes(id) ON DELETE RESTRICT,
    name TEXT UNIQUE NOT NULL,            -- DNS-safe slug (e.g., "my-api")
    deploy_type TEXT NOT NULL,            -- 'image' | 'git' | 'compose'
    image_name TEXT,                      -- For 'image' type
    git_repo_url TEXT,                    -- For 'git' type (https:// or ssh:// only)
    git_branch TEXT DEFAULT 'main',
    git_credential_enc TEXT,              -- Deploy key (ssh://) or access token (https://) for private repos
    build_method TEXT DEFAULT 'dockerfile', -- 'dockerfile' | 'railpack'
    compose_yaml TEXT,                    -- For 'compose' type
    compose_service TEXT,                 -- For 'compose' type: service that receives proxied traffic
    container_port INTEGER NOT NULL DEFAULT 8080,
    healthcheck_path TEXT DEFAULT '/',
    webhook_secret_enc TEXT,              -- Needed raw for HMAC verification, so encrypted, not hashed
    active_container_id TEXT,
    active_deployment_id TEXT,
    status TEXT NOT NULL DEFAULT 'stopped', -- 'running' | 'stopped' | 'building' | 'failed' | 'unhealthy'
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE volumes (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    name TEXT NOT NULL,                   -- Docker named volume: forgenerd-<app_name>-<name>
    mount_path TEXT NOT NULL,             -- Absolute path inside the container
    UNIQUE(app_id, name)
);

CREATE TABLE domains (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    hostname TEXT UNIQUE NOT NULL,        -- e.g., "api.example.com"
    force_https BOOLEAN NOT NULL DEFAULT 1,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE env_vars (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value_enc TEXT NOT NULL,
    is_secret BOOLEAN NOT NULL DEFAULT 1, -- Controls masking in API responses only
    UNIQUE(app_id, key)
);

CREATE TABLE deployments (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    commit_sha TEXT,
    image_ref TEXT,                       -- Built/pulled image; reused for rollback
    status TEXT NOT NULL,                 -- 'queued' | 'building' | 'healthy' | 'failed' | 'rolled_back'
    log_path TEXT,                        -- Authoritative persisted build log on disk
    started_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at DATETIME
);

CREATE TABLE s3_targets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    bucket TEXT NOT NULL,
    region TEXT NOT NULL,
    access_key_enc TEXT NOT NULL,
    secret_key_enc TEXT NOT NULL
);

CREATE TABLE backup_jobs (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    s3_target_id TEXT NOT NULL REFERENCES s3_targets(id) ON DELETE CASCADE,
    driver TEXT NOT NULL,                 -- 'postgres' | 'mysql' | 'volume'
    compose_service TEXT,                 -- Which service to dump, for compose apps
    volume_id TEXT REFERENCES volumes(id) ON DELETE CASCADE, -- For 'volume' driver
    db_name TEXT,                         -- For 'postgres' / 'mysql'
    db_user TEXT,
    db_password_enc TEXT,
    cron_expr TEXT NOT NULL,              -- e.g., "0 2 * * *"
    retention_count INTEGER NOT NULL DEFAULT 7,
    enabled BOOLEAN NOT NULL DEFAULT 1
);

CREATE TABLE notification_targets (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,                   -- 'discord' | 'slack' | 'generic'
    url_enc TEXT NOT NULL,                -- Webhook URLs embed secrets
    enabled BOOLEAN NOT NULL DEFAULT 1
);
