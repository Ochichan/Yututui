# Security Policy

## Reporting a vulnerability

Please report vulnerabilities privately via
[GitHub private vulnerability reporting](https://github.com/Ochichan/Yututui/security/advisories/new)
— **not** in a public issue or pull request.

This is a solo-maintained project. You should normally hear back within **7 days**;
confirmed issues are fixed in the next release (or an out-of-band release for anything
severe). Please leave reasonable time for a fix before public disclosure.

## Supported versions

YuTuTui! is a fast-moving public beta. Only the **latest release** receives security
fixes — older versions are not patched retroactively. Update paths for every install
method are listed in the in-app update notice and the README.

## What counts

Especially interesting areas, given what the app handles:

- Credential handling: auth cookies, API keys (e.g. Gemini, Last.fm/ListenBrainz),
  OAuth tokens, and the personal-data export allowlist.
- The local control endpoint / IPC surface (Unix socket permissions, token handling).
- The managed yt-dlp downloader and its SHA-256 verification.
- Release pipeline and packaging (Homebrew / Scoop / AUR artifacts).

- The encrypted sync vault and device pairing (see the threat model below).
- OpenSubsonic / Navidrome credential storage and per-request authentication.

Crashes without a security impact are ordinary bugs — a regular issue is perfect
for those.

## Encrypted sync — threat model

`ytt sync` replicates personal state between your own devices through a WebDAV folder.
This is the trust boundary it is built to hold, stated plainly so you can judge it.

**The WebDAV server is not trusted.** Every state object is encrypted on your device
before upload, to the age recipients of the devices you approved plus one offline
recovery recipient. A provider — or anyone who reads its disks or backups — sees object
sizes, counts and timestamps, and nothing else. Objects are additionally signed, so a
server that tampers with, replaces or rolls back an object cannot make your devices
accept it; it can still *withhold* data or serve a stale view, which surfaces as a sync
that does not advance rather than as silent data loss.

**The transport is not trusted either.** Endpoints must be HTTPS unless they are
loopback. Credentials are prompted with echo disabled, never accepted as command
arguments, and never written to status, audit or log output.

**Approved devices are trusted.** Pairing requires a 128-bit one-time code that expires
in ten minutes, and both sides display the same fingerprint for you to compare — comparing
it is what defeats an attacker who intercepts the code. Any approved device can read all
synced personal state; there are no partial-access devices.

**Revocation is forward-looking.** `ytt sync revoke` removes a device and re-locks
subsequent state so the removed device cannot read anything uploaded afterwards. It
cannot retract what that device already downloaded. Revoke as soon as a device is lost.

**The recovery kit is the last resort — and a full credential.** Anyone holding it can
decrypt your synced state. Store it off the synced machines, treat it like a password,
and understand that if every device *and* the kit are lost, the data is unrecoverable:
there is no server-side key escrow and the maintainer cannot reset anything.

**Out of scope.** A compromised operating-system account on a device that is already
approved: at that point the attacker has the device's own keys. Local disk encryption is
your OS's job, not this feature's.

## Verifying release artifacts

Every release ships a `checksums.txt` (SHA-256) and GitHub build-provenance
attestations for its artifacts:

```sh
# Checksums (download the artifact and checksums.txt into the same directory):
sha256sum -c --ignore-missing checksums.txt        # macOS: shasum -a 256 -c

# Provenance — proves the artifact was built by this repository's release workflow:
gh attestation verify yututui-linux-x64.tar.gz --repo Ochichan/Yututui
```

To avoid running an installer fetched from a moving branch, pin it to the latest
release instead of `main`:

```sh
curl -fsSL https://github.com/Ochichan/Yututui/releases/latest/download/install.sh | bash
```
