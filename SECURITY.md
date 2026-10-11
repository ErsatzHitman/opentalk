# Security policy

## Reporting a vulnerability

Please **do not** open a public issue for security problems. Use GitHub's
[private vulnerability reporting](https://github.com/ErsatzHitman/opentalk/security/advisories/new) instead, with steps
to reproduce and the OpenTalk version (Settings → About). You'll get an answer as soon as possible; fixes ship as a new
release.

## What OpenTalk protects

- **API keys** are encrypted with Windows DPAPI (current user) in `%APPDATA%\OpenTalk\secrets.json`, never logged, and
  sent only to the provider they belong to, over HTTPS.
- **Audio** is never written to disk and is sent only to the speech-to-text provider you configured.
- **The clipboard** used for pasting is excluded from Windows clipboard history and restored afterwards.
- There is no telemetry and no update check: OpenTalk only contacts the providers you configure.

## For contributors

Never commit real API keys. Tests that call a live provider read the key from the environment
(`OPENTALK_SECRET_<PROVIDER>`, for example `OPENTALK_SECRET_OPENROUTER`); `.env`, `*.key` and `*.secret` are git-ignored.
