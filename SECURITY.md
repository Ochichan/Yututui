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

Crashes without a security impact are ordinary bugs — a regular issue is perfect
for those.

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
