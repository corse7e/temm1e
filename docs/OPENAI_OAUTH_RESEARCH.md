# OpenAI OAuth for Codex — Research Document

**Branch:** `oauth_openai`
**Date:** 2026-03-11
**Status:** Research / Exploration
**Goal:** Enable SkyClaw users to authenticate with their ChatGPT Plus/Pro subscription via OAuth instead of API keys — same as OpenClaw does.

---

## Why This Matters

Currently SkyClaw requires users to have an OpenAI API key (pay-per-token billing). With Codex OAuth, users with a **ChatGPT Plus ($20/mo) or Pro ($200/mo) subscription** can use their subscription's included API access — no separate API billing needed. This is how OpenClaw works today.

---

## The OAuth Flow (Reverse-Engineered from OpenClaw + Codex CLI)

### Endpoints

| Endpoint | URL |
|----------|-----|
| **Authorization** | `https://auth.openai.com/oauth/authorize` |
| **Token exchange** | `https://auth.openai.com/oauth/token` |
| **API (Responses)** | `https://api.openai.com/v1/responses` |
| **API (Chat Completions)** | `https://api.openai.com/v1/chat/completions` |

### Client ID

```
app_EMoamEEZ73f0CkXaXp7hrann
```

This is a **public client ID** used by Codex CLI. OpenClaw, Roo Code, OpenCode, and other third-party tools all reuse this same client ID. There is no official OpenAI registration process for third-party OAuth clients yet — the community consensus is to reuse the Codex client ID.

> **Risk:** OpenAI could revoke or restrict this client ID at any time. No official guidance exists for third-party usage. See "Open Questions" below.

### PKCE Parameters

| Parameter | Value |
|-----------|-------|
| **Method** | S256 (SHA-256) |
| **Code verifier** | 32 random bytes → base64url encoded |
| **Code challenge** | SHA-256(verifier) → base64url encoded |

### OAuth Scopes

```
openid profile email offline_access
```

**Critical issue:** These identity-only scopes are what Codex CLI requests. However, for actual API access, the token also needs:
- `model.request` — permission to call models
- `api.responses.write` — permission to use the Responses API

OpenClaw hit this exact bug (issues #26801, #36660): OAuth succeeds but API calls fail with 403 because the token lacks API scopes. The fix is a **post-login scope probe** — validate the token can actually make API calls immediately after OAuth, fail early if not.

### Authorization URL (Full)

```
https://auth.openai.com/oauth/authorize
  ?client_id=app_EMoamEEZ73f0CkXaXp7hrann
  &redirect_uri=http://127.0.0.1:{PORT}/auth/callback
  &response_type=code
  &scope=openid+profile+email+offline_access
  &state={random_state}
  &code_challenge={challenge}
  &code_challenge_method=S256
  &id_token_add_organizations=true
  &codex_cli_simplified_flow=true
```

### Token Exchange (POST)

```
POST https://auth.openai.com/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=authorization_code
&code={auth_code}
&redirect_uri=http://127.0.0.1:{PORT}/auth/callback
&client_id=app_EMoamEEZ73f0CkXaXp7hrann
&code_verifier={verifier}
```

**Response:**
```json
{
  "access_token": "eyJhb...",     // JWT
  "refresh_token": "ort_abc...",
  "id_token": "eyJhb...",         // JWT with email, org info
  "token_type": "Bearer",
  "expires_in": 3600
}
```

### Token Refresh

```
POST https://auth.openai.com/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token
&refresh_token={refresh_token}
&client_id=app_EMoamEEZ73f0CkXaXp7hrann
```

### Using the Token for API Calls

Replace the API key with the OAuth access token:

```
Authorization: Bearer {access_token}
```

This works with both `/v1/chat/completions` and `/v1/responses` endpoints. The token is a JWT that OpenAI validates server-side.

---

## How OpenClaw Implements It

### File Structure

```
src/commands/openai-codex-oauth.ts          — Core OAuth flow (PKCE, browser, callback)
src/commands/auth-choice.apply.openai.ts    — Onboarding integration
src/commands/models/auth.ts                 — `openclaw models auth login --provider openai-codex`
src/agents/auth-profiles/oauth.ts           — Token refresh (refreshOAuthTokensForProfile)
src/agents/model-auth.ts                    — Token injection into API requests
```

### Credential Storage

```
~/.openclaw/credentials/oauth.json          — Raw OAuth tokens (import source)
~/.openclaw/agents/<agentId>/agent/auth-profiles.json  — Per-agent profiles
```

**Profile format:**
```json
{
  "openai-codex:user@email.com": {
    "type": "oauth",
    "access": "eyJhb...",
    "refresh": "ort_abc...",
    "expires": 1710180000,
    "email": "user@email.com",
    "accountId": "org-abc123"
  }
}
```

### Key Behaviors

1. **Auto-refresh:** Before each API call, checks `expires`. If within 5 minutes of expiry, refreshes under a file lock.
2. **Race condition handling:** Multiple agents sharing the same credential can cause `refresh_token_reused` errors (issue #26322). Fix: single-writer lock.
3. **Profile keying:** By email (`openai-codex:<email>`) not just `openai-codex:default` — supports multiple accounts.
4. **Default model:** `openai-codex/gpt-5.3-codex` (Codex-specific model variant).
5. **Local callback:** `http://127.0.0.1:1455/auth/callback` — binds a temporary HTTP server.
6. **Headless fallback:** If localhost binding fails, shows the auth URL and asks user to paste the redirect URL/code manually.

---

## Proposed SkyClaw Implementation Plan

### Phase 1: OAuth Flow (New Module)

**New file:** `crates/skyclaw-providers/src/openai_oauth.rs`

```rust
pub struct OpenAIOAuth {
    client_id: String,        // "app_EMoamEEZ73f0CkXaXp7hrann"
    redirect_port: u16,       // Dynamic port, default 1455
    tokens: Option<OAuthTokens>,
}

pub struct OAuthTokens {
    access_token: String,
    refresh_token: String,
    expires_at: u64,          // Unix timestamp
    email: String,
    account_id: String,
}
```

**Flow:**
1. Generate PKCE verifier + challenge (ring or sha2 crate)
2. Generate random state
3. Build authorize URL
4. Bind temporary HTTP server on `127.0.0.1:{port}` for callback
5. Open browser (or print URL for headless/Telegram)
6. Wait for callback with auth code
7. Exchange code for tokens at token endpoint
8. Extract email + accountId from id_token JWT (decode without verification — OpenAI signed)
9. Store tokens to `~/.skyclaw/oauth.json`
10. Validate token by making a test API call

### Phase 2: Token Management

**Token refresh:** Before each `Provider::complete()` call, check expiry. If within 5 min, refresh. Use `tokio::sync::Mutex` as file lock equivalent.

**Storage:** `~/.skyclaw/oauth.json`
```json
{
  "openai-codex": {
    "access_token": "...",
    "refresh_token": "...",
    "expires_at": 1710180000,
    "email": "user@email.com",
    "account_id": "org-abc123"
  }
}
```

### Phase 3: Provider Integration

Modify `OpenAICompatProvider` to accept either:
- `api_key` (existing) — `Authorization: Bearer sk-...`
- `oauth_token` (new) — `Authorization: Bearer eyJhb...`

The provider doesn't care which — both go in the same header. The difference is how they're obtained and refreshed.

### Phase 4: User Experience

**CLI:**
```
skyclaw auth login --provider openai-codex    # Opens browser for OAuth
skyclaw auth status                            # Shows auth state
skyclaw auth logout                            # Clears tokens
```

**Telegram (headless):**
```
/auth openai                                   # Bot sends OAuth URL
User clicks → authorizes → gets code
User pastes code back to bot
Bot exchanges code for tokens
```

**Config (`skyclaw.toml`):**
```toml
[provider]
name = "openai-codex"
auth = "oauth"                # vs "api_key" (default)
model = "gpt-5.2"
```

### Phase 5: Device Code Flow (Stretch)

For environments where neither browser redirect nor URL pasting works:
```
skyclaw auth login --device-code
```
Uses OpenAI's device code flow (beta) — user gets a code, enters it at openai.com/device.

---

## Open Questions

1. **Client ID legitimacy:** Is reusing `app_EMoamEEZ73f0CkXaXp7hrann` sanctioned by OpenAI? No official docs. Community consensus is "it works, everyone uses it." Risk: could be blocked.

2. **Scope requirements:** The identity scopes (`openid profile email offline_access`) may not be sufficient. OpenClaw hit bugs where tokens lacked `model.request` and `api.responses.write`. Need to test if the Codex client ID grants these scopes automatically or if they need to be requested explicitly.

3. **Model availability:** Does OAuth give access to the same models as API keys? Or is it limited to Codex-specific models (e.g., `gpt-5.3-codex`)? Need to test.

4. **Rate limits:** OAuth tokens may have different rate limits than API keys (subscription-based vs. pay-per-token). Need to verify.

5. **Telegram challenge:** Our primary interface is Telegram — no browser on the server. The headless flow (print URL → user pastes code) works but is clunky. Device code flow would be better but is beta.

6. **Token lifetime:** Access tokens expire in ~1 hour. Refresh tokens may expire or be single-use. Need robust refresh handling with retry logic.

7. **Legal/TOS:** Does using the Codex OAuth client ID for a third-party tool violate OpenAI's terms of service? No clear answer exists.

---

## Dependencies (Rust Crates)

| Crate | Purpose |
|-------|---------|
| `sha2` | SHA-256 for PKCE code challenge (already in tree via other deps) |
| `base64` | base64url encoding for PKCE (already in dependencies) |
| `axum` | Temporary HTTP server for OAuth callback (already in dependencies) |
| `reqwest` | HTTP client for token exchange (already in dependencies) |
| `jsonwebtoken` | JWT decoding for id_token (new, but small) |
| `rand` | Random bytes for verifier + state (already in dependencies) |

All major dependencies already exist in the workspace. Only `jsonwebtoken` would be new, and it's optional (can decode JWT payload without verification using base64 decode).

---

## Competitive Analysis

| Feature | OpenClaw | Codex CLI | SkyClaw (Proposed) |
|---------|----------|-----------|-------------------|
| OAuth PKCE | ✓ | ✓ | Planned |
| Device code flow | ✗ | ✓ (beta) | Stretch goal |
| Headless paste flow | ✓ | ✓ | Planned (Telegram) |
| Token auto-refresh | ✓ | ✓ | Planned |
| Multi-account | ✓ (by email) | ✓ | Planned |
| Scope validation | ✓ (post-fix) | ✓ | Planned (probe) |
| API key fallback | ✓ | ✓ | ✓ (existing) |

---

## Sources

- [OpenAI Codex Auth Docs](https://developers.openai.com/codex/auth/)
- [OpenClaw OAuth Docs](https://docs.openclaw.ai/concepts/oauth)
- [OpenClaw PR #32065 — Codex OAuth built-in](https://github.com/openclaw/openclaw/pull/32065)
- [OpenClaw Issue #26801 — Missing OAuth scopes](https://github.com/openclaw/openclaw/issues/26801)
- [OpenClaw Issue #36660 — Token lacks api.responses.write](https://github.com/openclaw/openclaw/issues/36660)
- [OpenClaw Issue #26322 — Refresh token race condition](https://github.com/openclaw/openclaw/issues/26322)
- [OpenCode Issue #3281 — Codex OAuth implementation](https://github.com/anomalyco/opencode/issues/3281)
- [OpenAI Community — ClientID best practices](https://community.openai.com/t/best-practice-for-clientid-when-using-codex-oauth/1371778)
- [OpenClaw DeepWiki — Model Providers & Auth](https://deepwiki.com/openclaw/openclaw/3.3-model-providers-and-authentication)
- [opencode-openai-codex-auth plugin](https://github.com/numman-ali/opencode-openai-codex-auth)
- [Codex Issue #2798 — Remote/headless OAuth](https://github.com/openai/codex/issues/2798)
