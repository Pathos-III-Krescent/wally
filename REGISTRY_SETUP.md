# Pathos Wally Registry: GitHub Login Setup

This guide switches our Wally registry (`https://wally.pathos.gg`) from one
shared API key to **each person logging in with their own GitHub account**.

When you're done:

- **Anyone in the `Pathos-III-Krescent` GitHub org** can download packages.
- **Only members of the `wally-publishers` team** can publish packages.
- **CI** can publish with its own key, but that key can't download anything.
- **Every request is logged** to a file: who, when, from where, what package, and whether it was allowed.
- Removing someone from the org cuts off their access within **60 seconds**.

You don't need to be a Rust or Docker expert. Follow the steps in order.

---

## 1. Create the GitHub OAuth App

This is what `wally login` uses to let people sign in with GitHub.

1. Go to <https://github.com/organizations/Pathos-III-Krescent/settings/applications>
   (Org **Settings** → **Developer settings** → **OAuth Apps**).
2. Click **New Org OAuth App** and fill in:
   | Field | Value |
   |---|---|
   | Application name | `Pathos Wally Registry` |
   | Homepage URL | `https://wally.pathos.gg` |
   | Authorization callback URL | `https://wally.pathos.gg` (required by GitHub but not used) |
   | **Enable Device Flow** | ✅ **Tick this box.** `wally login` won't work without it. |
3. Click **Register application**.
4. Copy the **Client ID** (looks like `Ov23li...`). It's not secret; you'll put it in two places.
5. Click **Generate a new client secret** and copy it straight away. GitHub only shows it once.
   **This is a secret.** Store it in your password manager.

Where these go:

| Value | Goes into |
|---|---|
| Client ID | `wally-index/config.json` (step 4) **and** the server's `WALLY_auth` (step 3) |
| Client secret | The server's `WALLY_auth` only (step 3). Never in a repo. |

## 2. Create the `wally-publishers` team

1. Go to <https://github.com/orgs/Pathos-III-Krescent/teams> → **New team**.
2. Name it `wally-publishers`. GitHub turns the name into a "slug"; check the
   team's URL ends in `/teams/wally-publishers`. The slug is what the server uses.
3. Add everyone who should be allowed to publish packages.

Optional: if only *some* of the org should be able to download packages, create a
`wally-readers` team too and set `reader-team="wally-readers"` (see step 3).
Make `wally-publishers` a **child team** of `wally-readers` so publishers can
also download. Without a reader team, the whole org can download.

## 3. Configure the server

### 3a. Create the server's GitHub token (`WALLY_github_token`)

The server uses this token to (1) push new package versions to `wally-index`
and (2) check whether a user is in the org/team.

Ideally create it on a dedicated bot account that's a member of the org. If you
use a person's account, the registry breaks when that person leaves the org.

1. Signed in as that account, go to <https://github.com/settings/personal-access-tokens/new>
   (**Settings** → **Developer settings** → **Personal access tokens** → **Fine-grained tokens**).
2. **Resource owner:** `Pathos-III-Krescent`.
3. **Expiration:** as long as your org allows. Put a reminder in the calendar a
   week before it expires. **When it expires, nobody can download.**
4. **Repository access:** *Only select repositories* → `wally-index`.
5. **Repository permissions:** **Contents → Read and write**. (Metadata → Read is added automatically.)
6. **Organization permissions:** **Members → Read-only**.
7. Generate and copy the token (starts with `github_pat_`).
8. If the org requires approval for fine-grained tokens, an org owner must approve it at
   <https://github.com/organizations/Pathos-III-Krescent/settings/personal-access-token-requests>.

### 3b. Create the CI publish key (optional)

Only needed if CI publishes packages. On any machine with OpenSSL:

```sh
openssl rand -hex 32
```

Copy the output. This is a secret.

### 3c. Fill in the server's `.env`

On the server, next to `docker-compose.yml`, edit `.env` (use `.example.env` as
a template). **Never commit `.env`.**

```sh
# One line. Replace the two YOUR_... values.
WALLY_auth={type="github-org",value={client-id="YOUR_OAUTH_CLIENT_ID",client-secret="YOUR_OAUTH_CLIENT_SECRET",org="Pathos-III-Krescent",publisher-team="wally-publishers"}}

WALLY_github_token=github_pat_...
WALLY_ci_publish_key=<output of openssl rand -hex 32>
WALLY_legacy_api_key=<the OLD shared key - only during the switch, see step 6>
WALLY_audit_webhook=<optional Discord webhook URL>
```

All the settings:

| Variable | Required? | What it does |
|---|---|---|
| `WALLY_auth` | **Yes** | Login mode and GitHub settings. The server **refuses to start** without it. Options inside `value={...}`: |
| ↳ `client-id`, `client-secret` | Yes | From the OAuth App (step 1). |
| ↳ `org` | Yes | GitHub org whose members can download. |
| ↳ `publisher-team` | Yes | Team slug whose members can publish. |
| ↳ `reader-team` | No | If set, only this team (instead of the whole org) can download. |
| ↳ `cache-ttl-secs` | No | How long (seconds) a login/membership check is remembered. Default `60`, maximum `60`. |
| `WALLY_github_token` | **Yes** | Server token from step 3a. |
| `WALLY_ci_publish_key` | No | Lets CI publish. Can't download. |
| `WALLY_legacy_api_key` | No | **Temporary.** The old shared key keeps working (download + publish) so nobody breaks mid-switch. Remove it in step 6. |
| `WALLY_audit_webhook` | No | Discord webhook. Gets a message for every publish and every denied request. If Discord is down, requests still work. |
| `WALLY_audit_log_path` | Set by `docker-compose.yml` | Audit log file, `/app/audit/audit.log` in the container = `/var/log/wally/audit.log` on the server. |

Leaving an optional value empty (`WALLY_ci_publish_key=`) is the same as not setting it.

## 4. Point `wally login` at GitHub (`wally-index/config.json`)

In the `wally-index` repo, add one line to `config.json`: `github_oauth_id`
with the **Client ID** from step 1. Keep everything else that's already there.

```diff
 {
   "api": "https://wally.pathos.gg",
+  "github_oauth_id": "YOUR_OAUTH_CLIENT_ID"
 }
```

(Watch the comma on the line before it: JSON needs commas between entries but
not after the last one.)

**Don't do this before step 5's server deploy**, or anyone who runs `wally
login` will get a GitHub token that the old server doesn't accept.

## 5. How people log in, and how CI publishes

### Employees

From inside any project that uses our registry (its `wally.toml` has our
`registry = "https://github.com/Pathos-III-Krescent/wally-index"`):

```sh
wally login
```

Wally prints a link and an 8-character code, and opens your browser. Paste the
code, click **Authorize**, and wait for `Authorization successful!`. That's it:
`wally install` and `wally publish` now use your GitHub account. The login is
saved in `~/.wally/auth.toml`; you only do this once per computer.

You need to be in the `Pathos-III-Krescent` org to download, and in
`wally-publishers` to publish. To sign out: `wally logout`.

### CI (GitHub Actions)

1. In the repo that publishes, go to **Settings** → **Secrets and variables** →
   **Actions** → **New repository secret**. Name it `WALLY_CI_PUBLISH_KEY`, and set
   the value to the CI key from step 3b.
2. Publish with:
   ```yaml
   - name: Publish to Wally
     run: wally publish --token "${{ secrets.WALLY_CI_PUBLISH_KEY }}"
   ```

The CI key **can't download packages**. If a CI job also needs `wally install`,
it needs a GitHub login token from an org member (e.g. a bot account).

## 6. Rollout order (nobody breaks mid-switch)

Do these in order. Each step is safe on its own.

| # | Step | What users notice |
|---|---|---|
| 1 | Steps 1–3: OAuth App, team(s), server token, CI key. | Nothing. |
| 2 | Note the **current shared key**: it's the `value` in today's `WALLY_auth={type="api-key",value="..."}`. | Nothing. |
| 3 | Merge the PR and let the image build. On the server, replace `.env` as in step 3c, putting the old shared key in `WALLY_legacy_api_key`. Then `docker compose pull && docker compose up -d`. | Nothing. Everyone's old key still works. Audit log shows them as `legacy-key`. |
| 4 | Check `docker compose logs wally-backend` shows `Using authentication mode: Github org (...)` and no errors. | Nothing. |
| 5 | Add `github_oauth_id` to `wally-index/config.json` (step 4). | Nothing yet. |
| 6 | Ask everyone to run `wally login` (step 5). | One-minute task each. |
| 7 | Switch CI to `WALLY_CI_PUBLISH_KEY`. | Nothing. |
| 8 | Watch the audit log (step 7 below) until there are **no `legacy-key` lines for a few days**. Chase whoever is still using it. | Nothing. |
| 9 | Delete the `WALLY_legacy_api_key` line from `.env` and run `docker compose up -d`. | Anyone who skipped `wally login` now gets `GitHub login required`. |
| 10 | Delete the old shared key everywhere: password managers, old CI secrets, notes. | Nothing. |

**Rolling back:** put the old `WALLY_auth={type="api-key",...}` line back in
`.env` and run the previous image. Each build is also tagged `main-<commit>`
in `ghcr.io/pathos-iii-krescent/wally`. Then either remove `github_oauth_id`
from `config.json`, or have people run `wally login --token <old key>` (with
`github_oauth_id` set, plain `wally login` opens GitHub instead of asking for a key).

## 7. Audit logs

Every download, metadata lookup, search, and publish writes **one line of JSON**
to the log file (shown spread out here so it's easier to read):

```json
{
  "event": "package.download",
  "timestamp": "2026-11-02T02:10:00Z",
  "github_username": "developer123",
  "github_user_id": 12345678,
  "package": "pathos/core",
  "version": "1.2.0",
  "status": 200,
  "source_ip": "203.0.113.10",
  "result": "allowed",
  "reason": null,
  "wally_version": "0.3.2"
}
```

| Field | Meaning |
|---|---|
| `event` | `package.download`, `package.metadata`, `package.search`, or `package.publish`. |
| `timestamp` | When it happened, in UTC. |
| `github_username` / `github_user_id` | Who did it. Or `ci-key`, `legacy-key`, or `anonymous` (no or invalid login), with no ID. |
| `package` / `version` | The package, e.g. `pathos/core` and `1.2.0`. Empty for searches. |
| `status` | HTTP status sent back: `200` OK, `401` not logged in, `403` not allowed, `404` not found. |
| `source_ip` | The client's IP (from Traefik's `X-Forwarded-For`). |
| `result` | `allowed`, `denied` (not logged in / not allowed), or `error` (e.g. package not found). |
| `reason` | Why it was denied or failed. |
| `wally_version` | Version of the Wally CLI that made the request, if it said. |

Tokens, keys, and `Authorization` headers are **never** logged.

### Where they are

1. **Container output** (stdout), mixed in with the server's other logs:
   ```sh
   docker compose logs wally-backend | grep '"event":"package.'
   ```
2. **A file on the server**, `/var/log/wally/audit.log`, which survives restarts and upgrades.
   Run these on the server itself:
   ```sh
   tail -n 50 /var/log/wally/audit.log
   # Only denied requests (needs jq):
   jq -c 'select(.result=="denied")' /var/log/wally/audit.log
   # Who is still using the old shared key?
   grep legacy-key /var/log/wally/audit.log
   ```
   The file only ever grows (about 300 bytes per request). To rotate it with
   logrotate, use `copytruncate` so the server keeps writing to the same file.
3. **Discord** (if `WALLY_audit_webhook` is set): publishes and denied requests.

## Troubleshooting

| Message | Fix |
|---|---|
| `GitHub login required. Run wally login` | Run `wally login`. |
| `GitHub auth was invalid` / `Github auth failed` | Login expired or was revoked. Run `wally login` again. |
| `<name> is not a member of the GitHub organization` | Add them to the org. Works within 60 seconds. |
| `<name> is not a member of the wally-publishers GitHub team` | Add them to the team (and make sure they accepted the invite). |
| `The CI key can only publish packages` | Expected: the CI key can't download. |
| `Could not check GitHub membership` | The server's `WALLY_github_token` expired or lacks **Members → Read-only**. |
| Server won't start: `missing field auth` | `WALLY_auth` isn't set in `.env`. This is on purpose. |
| Server won't start: `could not open audit log` | `/var/log/wally` is missing or not writable. See below. |

**About the `could not open audit log` error:** the server runs as user ID
1000, and `/var/log/wally` on the server must exist and be writable by it.
Create it once on the server (on every Swarm node the service can run on):
`sudo mkdir -p /var/log/wally && sudo chown 1000:1000 /var/log/wally`.

## Good to know

- Package **names, versions and dependencies** live in the public `wally-index`
  repo, and the Wally CLI reads them from there directly. So they stay visible
  to anyone. Only the package **contents** (the code) are protected by login.
- The registry website (`wally-registry-frontend`) doesn't send logins. It
  can't show our packages in this mode.
