-- History of backup executions (scheduled or manual), newest first in the API.
CREATE TABLE backup_runs (
    id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL REFERENCES backup_jobs(id) ON DELETE CASCADE,
    status TEXT NOT NULL,                 -- 'running' | 'succeeded' | 'failed'
    trigger TEXT NOT NULL,                -- 'schedule' | 'manual'
    object_key TEXT,
    size_bytes INTEGER,
    error TEXT,
    started_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at DATETIME
);

CREATE INDEX backup_runs_job ON backup_runs(job_id, started_at);
