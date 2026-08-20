# Security Policy

## Reporting a vulnerability

Please report vulnerabilities privately via
[GitHub private vulnerability reporting](https://github.com/Ochichan/Yututui/security/advisories/new)
— **not** in a public issue or pull request.

This is a solo-maintained project. You should normally hear back within **7 days**;
confirmed issues are fixed in the next release (or an out-of-band release for anything
severe). Please leave reasonable time for a fix before public disclosure.

## Supported versions

YuTuTui! is a fast-moving project. Only the **latest release** receives security
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

**The WebDAV server is not trusted with your content.** Every state object is encrypted
on your device before upload, to the age recipients of the devices you approved plus one
offline recovery recipient. Objects are additionally signed, so a server that tampers
with, replaces or rolls back an object cannot make your devices accept it; it can still
*withhold* data or serve a stale view, which surfaces as a sync that does not advance
rather than as silent data loss.

**It does see structure.** Confidentiality covers object *contents*, not object *names*.
A provider — or anyone reading its disks, backups or request logs — observes sizes,
counts and timestamps, and also the object keys, which encode your dataset id, your
device ids, operation sequence ranges, membership epoch hashes, checkpoint hashes and
pairing invite ids. From those alone an observer can infer how many devices you use, how
often each one syncs, and roughly how much you listened between two points in time. If
that metadata matters to you, host the WebDAV endpoint yourself.

**The transport is not trusted either.** Endpoints must be HTTPS unless they are
loopback. Credentials are prompted with echo disabled, never accepted as command
arguments, and never written to status, audit or log output.

**Approved devices are trusted.** Pairing requires a 128-bit one-time code that expires
in ten minutes and is single-use; that code is what an attacker must obtain. Before you
approve, the existing device shows the joining device's name and key fingerprint. Note
the current limit: the joining device does not display that fingerprint back to you, so
you cannot yet compare it on two screens — approval rests on the code plus the name you
recognise. Any approved device can read all synced personal state; there are no
partial-access devices.

**Revocation is forward-looking.** `ytt sync revoke` removes a device and re-locks
subsequent state so the removed device cannot read anything uploaded afterwards. It
cannot retract what that device already downloaded. Revoke as soon as a device is lost.

**The recovery kit is a full credential.** Anyone holding it can decrypt your synced
state. Store it off the synced machines and treat it like a password. There is no
server-side key escrow and the maintainer cannot reset anything, so if every device *and*
the kit are lost, the data is gone.

**Restoring from the kit is not wired up yet.** Setup requires the kit and verifies it,
`ytt sync recovery export` re-checks and copies it, and the underlying operation exists
and is tested — but no CLI or TUI command currently rebuilds a vault from a kit alone.
Until that lands, treat the kit as the material you will need later rather than as a
restore you can perform today, and keep at least one approved device.

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
