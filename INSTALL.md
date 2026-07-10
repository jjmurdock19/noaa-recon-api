# Installing noaa-recon-api

This is the plain-language version. If you already know your way around
Linux servers and just want the commands, see [README.md](README.md#manual-setup)
instead — this page is for "walk me through it."

## The one-line install

On a fresh Linux machine (Fedora, Rocky/RHEL/CentOS, Debian, Ubuntu, or
anything running the Nix package manager), log in, and paste this:

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/jjmurdock19/noaa-recon-api/main/install.sh)"
```

That downloads and runs `install.sh` from this repo. Nothing happens to
your system until you run it — read it first if you'd rather not pipe a
script straight into a shell, that's a completely reasonable instinct:

```bash
curl -fsSL https://raw.githubusercontent.com/jjmurdock19/noaa-recon-api/main/install.sh -o install.sh
less install.sh      # read it
bash install.sh      # run it
```

Note the command is `bash -c "$(curl ...)"`, **not** `curl ... | bash`.
That's deliberate, not a typo: the installer asks interactive questions,
which means it needs to read your keystrokes from stdin — but a plain
`curl | bash` pipe hands bash *the script itself* over stdin, leaving
nothing for the prompts to read (and on some systems it fails outright
with `curl: (23) Failure writing output to destination`, because bash
stops reading the piped script partway through). `bash -c "$(...)"`
downloads the whole script into memory first, so stdin stays free for
your answers. If you want to pass flags (see below) with this form, add
a placeholder for `$0` right after the script:
`bash -c "$(curl -fsSL .../install.sh)" bash --update`.

You do **not** need to run it as `root`. If it needs to install packages
or write system files, it'll ask for your password via `sudo` at that
point, the same way any normal Linux install would.

It opens with a banner, then walks you through a short list of
questions, described below. Answer them (or just press Enter to accept
the sensible default shown in brackets), and in a few minutes you'll have
a running, self-updating copy of the API.

## What it actually does

Under the hood, `install.sh`:

1. Figures out whether your system uses `dnf` (Fedora/RHEL/Rocky/CentOS),
   `apt` (Debian/Ubuntu), or the Nix package manager, and installs git,
   Python 3, and a compiler with whichever one it finds.
2. Downloads this repository to a folder you choose (`/opt/noaa-recon-api`
   by default).
3. Creates an isolated Python environment inside that folder (a
   "virtualenv") and installs the API's dependencies into it — this
   never touches your system's Python packages.
4. Sets up a **systemd service** so the API starts automatically on boot
   and restarts itself if it ever crashes — the standard, boring way
   every other background service on a Linux box works. No Docker, no
   custom process manager, nothing to remember to restart by hand.
5. If you're putting this on a domain, optionally reconfigures nginx or
   Apache to forward traffic to it, and can request a free HTTPS
   certificate for you (via Let's Encrypt / certbot).
6. Downloads the storm-track and hurricane-recon-flight archives the API
   serves, and installs three nightly timers: two that keep them current
   forever, plus one that clears out stale cached netCDF files, all
   without you doing anything.
7. Installs a `noaa-recon-api` command so you can check on it later
   (see "Living with it" below).

## The questions it asks, and what to answer

**"Where should noaa-recon-api live?"**
The folder it gets installed into. The default (`/opt/noaa-recon-api`) is
fine for almost everyone — just press Enter.

**"System user to run the API service as"**
For security, the API shouldn't run as `root`. If you press Enter, it'll
offer to create a dedicated, low-privilege user called `noaa-recon-api`
that can't even log in — this is the recommended choice unless you
already have a specific user in mind (e.g. the same user your webserver
runs as).

**"How will this API be reached?"** — pick one:

- *Just this machine* — the API only answers on `127.0.0.1` (localhost).
  Good for trying it out, or if something else on the same machine talks
  to it directly.
- *My local network* — reachable from any device on your LAN by this
  machine's IP address and a port number, no domain needed. Good for a
  home lab or office network.
- *A domain name over the internet* — reachable at a real URL like
  `https://api.yourdomain.com`. Pick this if you want other people or
  websites to be able to use it.

If you pick the domain option, it'll also ask:

- **The domain itself** (e.g. `api.yourdomain.com`). This has to already
  point at this machine in your DNS — the installer doesn't manage DNS
  for you, only what happens once traffic arrives.
- **Dedicated subdomain vs. a path on an existing site.** If you already
  have a website running on this machine and want the API to live at
  `yourdomain.com/api/` instead of its own subdomain, pick the second
  option. For safety, the installer won't blindly edit a config file it
  didn't create — it'll write the one snippet you need and tell you
  exactly which line to add and to which existing file.

**"Reconfigure nginx/Apache..." / webserver install offer**
If it finds nginx or Apache already running, it asks permission before
touching it. If it finds neither, it offers to install and set up nginx
(the more common choice) for you.

**"Set up free HTTPS via Let's Encrypt?"**
Only asked if you picked the domain option. Requires your domain's DNS to
already be pointed at this machine and ports 80/443 to be reachable from
the internet — if either isn't true yet, say no for now and re-run
`sudo certbot --nginx -d yourdomain.com` later once it is.

**Admin console username/password**
The API ships with a small web dashboard (cache stats, database browser,
force-refresh buttons) gated behind a login. The installer generates a
random password for you by default — **it's shown once, at the very
end, so save it somewhere** (a password manager, a sticky note, whatever
you'd trust with any other admin password). This account becomes your
first **superuser** — from the console's API management pane you can
create additional superuser/moderator accounts (their own username/
password) or plain API-key tokens for other people, each tracked in the
login/usage logs.

**"Require an API token for the public data endpoints?"**
Off by default — the satellite/storms/recon/tdr/raw endpoints stay open
exactly like today, so anyone can drop this API straight into a Leaflet
map with no setup. Say yes if you'd rather track/restrict who calls your
instance; `/v1/health` and the admin console always stay reachable
without a token either way. This can be flipped later from the admin
console without reinstalling.

**"Build the storm-track and recon MET archives now?"**
These are the actual databases the API serves data from. The storm-track
one takes about 10 seconds. The full recon archive (every hurricane
hunter flight since 2011) can take **several hours** if you ask for the
whole thing — you'll be asked separately about that, with the fast
option (current + previous hurricane season only) as the default.

## After it finishes

The final screen prints the URL your API is reachable at, a link to the
interactive API docs (`/docs`), and your admin login. From then on:

```bash
noaa-recon-api status      # is it running? quick health check
noaa-recon-api logs        # watch the logs live
noaa-recon-api update      # pull the latest code from GitHub and restart
noaa-recon-api restart     # just restart it
noaa-recon-api uninstall   # remove everything this installer set up
```

## Staying up to date

Re-run the same one-liner any time. If it detects an existing
installation, it'll offer to **update** (pull the latest code and
restart — the quick path) or **reconfigure** (re-run the full wizard, in
case you want to change the domain, port, etc.) instead of installing a
second copy.

### Do I need to publish GitHub Releases for this?

No. Updates work by tracking the `main` branch directly (`git fetch` +
`git reset --hard origin/main`, the same thing `noaa-recon-api update`
does under the hood) — whatever's on `main` *is* the latest version.
There's no separate packaging or release step to remember, and nothing
to keep in sync. Tagging occasional releases is still fine if you want a
changelog people can point to, but it isn't required for anything the
installer or the update command does.

This is also why there's no `.rpm`/`.deb` package or a Nix flake for this
project (yet) — those all require you to publish and version something,
which is a lot of ongoing ceremony for what's currently a one-maintainer
project deployed on a handful of machines. If that changes, revisit it.

## Uninstalling

```bash
noaa-recon-api uninstall
```

This stops and removes the systemd service, the nightly update timers,
any nginx/Apache configuration it wrote, and the `noaa-recon-api` command
itself. It will **ask separately** before deleting the installed code and
databases — say no if you just want to stop the service but keep the
downloaded storm-track/recon archives around for next time.

## Troubleshooting

**"SELinux is blocking nginx from reaching the API"** (Fedora/RHEL/Rocky)
The installer already runs `setsebool -P httpd_can_network_connect 1`
automatically when it configures a webserver on these systems, which is
the fix for this specific symptom. If you still see `502 Bad Gateway`
after that, check `sudo journalctl -u noaa-recon-api` to confirm the API
process itself is actually running.

**"It says the port is already in use"**
Something else on the machine is already using port 8000. Re-run the
installer and choose Reconfigure, then pick a different port when asked.

**"certbot failed"**
Almost always means the domain's DNS A record isn't pointing at this
machine yet, or ports 80/443 aren't reachable from the internet (check
your router/cloud firewall, not just this machine's local firewall). Fix
that, then run the command the installer printed
(`sudo certbot --nginx -d yourdomain.com`) again — no need to re-run the
whole installer.

**"I want to change my answers"**
Re-run the installer and choose **Reconfigure** — your previous answers
are pre-filled as the defaults, so you only need to change what's
actually different.

## Windows (local testing)

`install.sh` above is for a real Linux server. If you're on Windows and
just want to try the API out locally, use `install.ps1` instead — same
idea, deliberately smaller scope:

```powershell
irm https://raw.githubusercontent.com/jjmurdock19/noaa-recon-api/main/install.ps1 | iex
```

(That's PowerShell's `curl | bash` equivalent — download and run in one
step. It reads keystrokes from the real console rather than the download
pipe, so — unlike the bash installer — there's no gotcha here; it's just
as safe to pipe directly. Prefer to read it first? `irm ... -OutFile
install.ps1`, open it, then `.\install.ps1`.)

It asks fewer questions than the Linux installer on purpose:

- **Where to install** (defaults to `%LOCALAPPDATA%\noaa-recon-api` — no
  admin rights needed).
- **Localhost-only vs. LAN-accessible** and a port. That's it for
  networking — no domain, no reverse proxy, no HTTPS, no firewall rules.
  This installer is for testing on your own machine, not serving the
  internet.
- The same admin-console username/password and storm/recon-archive
  questions as the Linux installer.

It installs a `noaa-recon-api` command (open a **new** terminal window
after install for it to show up on PATH):

```powershell
noaa-recon-api start       # launch it in the background
noaa-recon-api stop        # stop it
noaa-recon-api status      # is it running?
noaa-recon-api logs        # tail the logs
noaa-recon-api update      # pull the latest from GitHub and restart
noaa-recon-api uninstall   # remove everything
```

**On purpose, it does not run as a Windows Service or start itself at
login** — you run `noaa-recon-api start` when you want to test it, same
as you'd run any other local dev server. If you later want it to survive
reboots and restart itself on a crash the way the Linux systemd service
does, that's a genuinely different tool (a registered Service or
Scheduled Task) — ask if you want that built out; it wasn't in scope here
by request.

Prerequisites (git, Python 3.9+) are installed via `winget` if missing
and you approve it — `winget` ships with Windows 10 (1809+) and 11 by
default. No `winget`? The installer prints direct download links instead.

## Doing it by hand instead

If you'd rather not run a script at all, every step above corresponds to
a plain command — see [README.md's "Manual setup" section](README.md#manual-setup)
for the full copy-paste walkthrough (clone, venv, systemd unit, nginx
snippet, ingestion scripts, timers) with no wizard in between.
