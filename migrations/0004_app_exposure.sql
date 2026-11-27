-- Whether an app is served publicly through the proxy. Exposed apps get
-- <name>.<root_domain> (when a root domain is set) plus their custom domains.
ALTER TABLE apps ADD COLUMN exposed BOOLEAN NOT NULL DEFAULT 1;
