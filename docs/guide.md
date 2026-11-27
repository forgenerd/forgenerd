# forgenerd guide

Everything you need to install, run and maintain forgenerd.

## Requirements

- Linux (x86_64 or arm64) with systemd
- Docker Engine with the Compose plugin (and buildx for git builds)
- Ports 80 and 443 free for Caddy

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/forgenerd/forgenerd/main/scripts/install.sh | sudo bash
```

The installer creates a `forgenerd` system user, puts the binary in `/usr/local/bin`,
keeps data in `/var/lib/forgenerd` and starts the `forgenerd` systemd service. On small
machines it also adds a swapfile (skip with `--no-swap`). Run it with `--help` for every
option.

When it finishes it prints a one-time setup token. You can also find it with
`journalctl -u forgenerd`.

### Open the panel

The panel listens on `127.0.0.1:4367` and is not reachable from the internet. Open an
SSH tunnel from your machine:

```sh
ssh -L 4367:127.0.0.1:4367 you@your-server
```

Then browse to <http://localhost:4367>, enter the setup token and create the admin
account.

### Use your own domain

1. Point a wildcard DNS record at the server: `*.example.com` as an A (and AAAA) record.
2. In **Settings**, set the root domain to `example.com`.

The panel is then served at `https://hero.example.com`, and each exposed app at
`https://<app>.example.com`. Certificates are issued automatically. Apps can also have
custom domains, set on their **Domains** tab.

## Deploy an app

1. Create a project, then an app inside it.
2. Choose how it is deployed: an image, a git repository (public, or private with an
   access token or deploy key), or a Compose file.
3. Set the port the app listens on and, if needed, a health check path, environment
   variables, volumes and resource limits on the **Config** tab.
4. Click **Deploy**. The build log streams live.

For push-to-deploy, copy the webhook URL and secret from the app's **Deployments** tab
into your git host. Pushes to the configured branch trigger a deploy.

Apps reach each other on the internal network by app name, for example
`postgres://main-db:5432`.

## Backups

Add an S3 target on the **Backups** page, then create backup jobs:

- **Postgres** and **MySQL** jobs dump the database with `pg_dump` or `mysqldump` inside
  the database container.
- **Volume** jobs archive a named volume.

Jobs run on a cron schedule (UTC) and keep the newest N backups. Open a job's history and
click **Restore** to replace the current data with a backup. Volume restores stop the app
while they run.

### Backing up forgenerd itself

On the **Maintenance** page, choose an S3 target under **Panel backup**. This backs up the
panel's database: apps, domains, environment variables, jobs and settings. Secrets in it
are encrypted with the master key, which is never uploaded. **Copy
`/var/lib/forgenerd/master.key` somewhere safe.** Without it a panel backup cannot be
decrypted.

To rebuild a lost server:

```sh
# On the new server, after installing forgenerd:
sudo systemctl stop forgenerd
sudo install -o forgenerd -g forgenerd -m 600 master.key /var/lib/forgenerd/master.key
sudo -u forgenerd forgenerd restore-db /path/to/forgenerd.db.gz
sudo systemctl start forgenerd
```

Then redeploy each app, and restore its data from the app backups.

## Maintenance

The **Maintenance** page shows disk usage, removes old app images and build cache (on a
schedule or on demand, always keeping the newest few images per app for rollback), and
checks for new releases.

### Upgrade

Re-run the installer. The binary is replaced in place, database migrations run on
start, and your data and secrets are kept. Running apps and Caddy keep serving while
forgenerd restarts.

### Forgot your password

```sh
sudo -u forgenerd forgenerd reset-password you@example.com
```

It reads the new password from standard input and signs out every session.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `FORGENERD_DATA_DIR` | `/var/lib/forgenerd` | Database, master key, build and backup scratch space |
| `FORGENERD_LISTEN` | `127.0.0.1:4367` | Panel address. `0.0.0.0:4367` makes it public over plain HTTP |
| `FORGENERD_MASTER_KEY` | read from `master.key` | Base64 32-byte key that encrypts secrets |
| `FORGENERD_REPO` | `forgenerd/forgenerd` | GitHub `owner/name` that updates are installed from and checked against |
| `RUST_LOG` | `info` | Log level |

## Building from source

```sh
(cd web && pnpm install && pnpm build)   # the UI is embedded into the binary
cargo build --release
```

Run the tests with `cargo test`; they use an in-process fake of Docker and Caddy.

