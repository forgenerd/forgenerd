# Security policy

forgenerd runs Docker on your server and holds credentials for your git hosts, registries
and S3 targets, so security reports are taken seriously.

## Reporting a vulnerability

Please do not open a public issue. Report it privately through GitHub instead:
**Security** tab of this repository, then **Report a vulnerability**.

Include what you found, how to reproduce it and the version affected
(`forgenerd --version`). You should get a reply within a few days. Once a fix is
released, the advisory is published and you are credited unless you prefer otherwise.

## Supported versions

Only the latest release gets security fixes. Upgrade by re-running the installer.

## Scope

In scope: the forgenerd binary, its REST API and web panel, the installer, and the
isolation it promises (the Compose policy, encrypted secrets, login rate limiting, apps
not publishing host ports).

Out of scope: vulnerabilities in the apps you deploy, in Docker or Caddy themselves, and
setups that deliberately expose the panel over plain HTTP with `--public-panel`.
