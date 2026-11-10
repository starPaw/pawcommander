# Product website

The public site is [pawcommander.capture402.xyz](https://pawcommander.capture402.xyz/). Its source lives in `website/` alongside the application. It is static HTML, CSS, and JavaScript, with no build dependencies, external fonts, analytics, wallet connections, or application backend.

## Preview and edit

```sh
python3 -m http.server 4173 --bind 127.0.0.1 --directory website
```

Open `http://127.0.0.1:4173/`. Edit `index.html`, `styles.css`, and `script.js`. Check desktop and mobile layouts, keyboard navigation, roadmap disclosures, and both copy-address buttons. The copy buttons require a secure context (HTTPS or localhost); their fallback selects the address for manual copying.

Keep release links, version, archive size, platform requirements, and first-launch notes aligned with the actual GitHub release. The six roadmap phases are an editorial snapshot of the development plan, not a live integration. Update their descriptions, status, and date after checking the current roadmap; keep planned capabilities separate from released ones. Do not publish internal task notes, account details, or private workspace links.

Donation addresses are literal text in the page. Copy buttons copy that visible text. Update both the address and its network label together, then check the copied value. The page does not initiate transactions.

## Server setup

The production site uses nginx with an independent virtual host. `deploy/pawcommander.nginx.conf` is the HTTPS configuration template. It expects:

- DNS for `pawcommander.capture402.xyz` pointing to the server.
- A Let's Encrypt certificate at `/etc/letsencrypt/live/pawcommander.capture402.xyz/`.
- HTTP-01 validation files served from `/var/www/pawcommander/acme`.
- nginx's existing Certbot TLS options and DH parameter files.
- Certificate auto-renewal with an nginx reload after successful renewal.

For a new server, first serve the ACME location over HTTP, obtain the certificate using the existing Certbot account, then install the HTTPS configuration. Run `nginx -t` before reloading. Do not replace unrelated virtual hosts.

## Publish an update

After checking the diff and preview:

```sh
bash scripts/deploy-website.sh <SSH host or user@host>
```

The script requires SSH access with a verified host key, noninteractive sudo on the destination, and local Node.js for the JavaScript syntax check. It uploads only an explicit list of static assets, verifies the archive checksum, and installs an immutable release under `/var/www/pawcommander/releases/`. It switches the `current` symlink atomically and compares the page served over HTTPS with the local HTML checksum. If that check fails, it restores the previous symlink. No application process or nginx reload is needed for normal content updates.

Previous releases stay available for manual rollback. On the server, create a new symlink to the chosen previous release and atomically rename it to `/var/www/pawcommander/current`. Check the public HTTPS page afterward. Keep deployment credentials and server-specific access notes outside Git.

Before reporting a release as published, check public HTTPS, the HTTP redirect, static assets, download links, roadmap toggles, and the donation buttons. Update the repository's Website field if the domain changes.
