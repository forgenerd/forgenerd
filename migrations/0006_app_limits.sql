-- Per-app resource limits (NULL = unlimited) and how long a new deployment may take to
-- pass its health check (NULL = the default of 30 seconds).
ALTER TABLE apps ADD COLUMN memory_limit_mb INTEGER;
ALTER TABLE apps ADD COLUMN cpu_limit REAL;
ALTER TABLE apps ADD COLUMN health_timeout_secs INTEGER;
