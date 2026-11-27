CREATE TABLE app_tags (
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    PRIMARY KEY (app_id, tag)
);

CREATE INDEX idx_app_tags_tag ON app_tags(tag);
