# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Open Uptime Bot is a Rust backend for uptime monitoring with Ntfy.sh push notifications. Clients (like the Pico W microcontroller) ping the server periodically; if pings stop, the server sends "down" notifications.

## Build Commands

```bash
# Development environment (Rust, Diesel CLI, espflash, picotool)
nix develop

# Run tests (in dev, always redirect to file so agents can review logs)
nix flake check -L &>/tmp/flake-check.log

# Build Docker image
nix build .#docker
docker load < result

# Database operations
./diesel_run.sh       # Run migrations
./recreate_db.sh      # Reset database

# Local development (builds docker image, resets DB, starts services, builds CLI)
source ./setup_local.sh
```

### Local Dev Gotchas

- **Rocket log level**: Successful requests aren't logged at default level. Use `curl .../api/v1/metrics | grep oubot_uptime` to verify heartbeats.
- **`nix develop -c`**: Use this for stateless commands (e.g., `nix develop -c oubot-cli me`). The CLI and espflash are both in the devShell.

## Rust conventions

- Use Rust edition 2024. Run `cargo fix --edition` before changing older editions in `Cargo.toml`.
- Apply `rustfmt.toml` before formatting: 131 columns for lines and width heuristics, with compressed function parameters.
- Group imports into as few single-line `use` statements as fit within 131 columns. After formatting, combine same-crate leftovers with another fitting statement.
- Run `kitty-review` before committing each changeset.
- Add tests only for high-value behavior and failure paths. Use existing checks for routine dependency and formatting changes.

## Architecture

### Core Components

- **src/main.rs** - Entry point, Rocket server setup, fairings, DB startup load
- **src/api/** - REST endpoints split into `core.rs` (health, heartbeat), `user.rs` (self-service), `admin.rs` (admin-only)
- **src/db/** - `mod.rs` has query functions, the `bb8` pool type, and the connection request guard. `models.rs` has Diesel ORM structs/enums (re-exported via `db::`). Pool setup is in `src/main.rs`.
- **src/background.rs** - Background task: monitors uptime states, triggers "down" notifications
- **src/notifications.rs** - Ntfy dispatch, duration formatting, i18n (locales), `utc_minute_of_day`
- **src/context.rs** - In-memory state (`Context`) with `RwLock<HashMap>` for users, tokens, uptime states
- **src/bauth.rs** - Bearer token authentication (`Authorization: token <token>`), IP-based rate limiting (5 req/sec fairing), auth failure logging for fail2ban
- **src/ntfy.rs** - Ntfy.sh HTTP client and user management
- **src/prom.rs** - Prometheus metrics collection
- **src/actions.rs** - Business logic for user/invite creation
- **cli/src/main.rs** - CLI dispatch; `commands.rs` (clap enums), `client.rs` (HTTP), `format.rs` (output formatting)

### State Management

In-memory HashMap cache backed by PostgreSQL. On startup, loads all users/states from DB into memory. API operations update both in-memory state and database.

### Database Tables

- **users** - Accounts with access_token, up_delay, language_code, ntfy_id, maint_window_start/end_utc
- **uptime_states** - Device status (uninitialized/up/down/paused), touched_at, state_changed_at, pre_pause_status
- **ntfy_users** - Ntfy.sh credentials per user
- **invites** - Invitation tokens for user registration (is_used tracks consumption)

### API Endpoints

- `GET /api/v1/up` - Client heartbeat (BAuth)
- `GET /api/v1/health` - Health check
- `GET /api/v1/metrics` - Prometheus metrics
- `POST /api/v1/users` - Create user (first admin needs no invite; subsequent users need invite token)
- `POST /api/v1/invites` - Create invite (AdminAuth)
- `GET /api/v1/invites` - List invites (AdminAuth)
- `DELETE /api/v1/invites/<id>` - Delete invite (AdminAuth)
- `GET /api/v1/me` - Current user info (BAuth)
- `POST /api/v1/me/regenerate-token` - Regenerate access token (BAuth)
- `GET|PATCH /api/v1/me/ntfy` - Ntfy settings (BAuth)
- `GET|PATCH /api/v1/me/language` - Language setting (BAuth)
- `POST /api/v1/me/pause` - Pause monitoring (BAuth)
- `POST /api/v1/me/unpause` - Resume monitoring (BAuth)
- `GET|PATCH /api/v1/me/settings` - Up delay + maintenance window (BAuth)
- `GET /api/v1/admin/users` - List all users (AdminAuth)
- `GET /api/v1/admin/users/<id>` - Get user (AdminAuth)
- `DELETE /api/v1/admin/users/<id>` - Delete user (AdminAuth)

### Notification Flow

1. Client sends `GET /api/v1/up` with bearer token
2. Server updates uptime state in memory and DB
3. If state changed (Down->Up or Up->Down after timeout), sends notification via Ntfy.sh
4. Duration messages are localized (Ukrainian/English) based on user's language_code

### Pause (Freeze/Thaw)

Pause freezes the uptime state — no state changes, no notifications while paused. Allowed from Up or Down. Saves `pre_pause_status` so unpause restores the exact state. Unpause refreshes `touched_at` and `state_changed_at` to now (fresh countdown). Pause/unpause persist to DB inside the write lock to prevent the race where `background_handle_down`'s deferred write could overwrite Paused with Down.

### Maintenance Window

Per-user daily UTC time range (minutes from midnight, stored on users table). During the window, `background_handle_down` skips the Down transition and `api_up` suppresses Connected/Restored notifications. Handles midnight-spanning windows (start > end means `now >= start || now < end`). No state changes occur — the device stays Up, and after the window ends the normal `touched_at + up_delay` check resumes naturally.

## Testing

Integration tests in `tests/` directory use Python + Nix test harness:

```bash
nix flake check -L  # Runs all NixOS integration tests
```

Test names are defined in `flake.nix` under `checks`. Each test has a `.nix` file in `tests/` with a corresponding script (`.py` or `.sh`) or inline Python testScript. Run `nix eval .#checks.x86_64-linux --apply 'x: builtins.attrNames x'` to list all test names.

Test infrastructure in `tests/lib/`:
- **config.nix** - Shared constants (ports, credentials, tier name)
- **infra.nix** - PostgreSQL + ntfy-sh service definitions
- **services.nix** - Imports infra.nix, adds oubot systemd service
- **ntfy-bootstrap.nix** - Shared ntfy admin setup script fragment
- **primary.nix** - Imports services.nix, sets env vars and packages for single-node tests
- **lib.nix** - Test runner (builds Python/bash test scripts, passes args to NixOS test)

Native tests import `primary.nix` (full stack). Docker E2E imports `infra.nix` directly (runs oubot via Docker container instead of systemd).

### CI Checks (also in `flake.nix`)

All linting runs via `nix flake check` — no separate lint workflow:
- **fmt** - `cargo fmt --check` (all 4 crates) + `alejandra` (Nix) + `black`/`isort` (Python)
- **clippy** / **clippy-cli** - `cargo clippy -- -D warnings` (server / CLI, crane-cached deps)
- **lint** - `shellcheck` + `flake8` + `deadnix` + route-guard-lint (rate-limit guard on every handler)
- **build-esp32** / **build-pico-w** - Cross-compilation build verification (dummy env vars)

## Configuration

Required `.env` variables:
- `NTFY_BASE_URL`, `NTFY_ADMIN_TOKEN`, `NTFY_USER_TIER` - Ntfy.sh integration
- `DATABASE_URL` - PostgreSQL connection

Server config in `Rocket.toml` (port 8080, `ip_header = "X-Forwarded-For"` for reverse proxy IP extraction).

## Security

- **Rate limiting**: IP-based via governor fairing (5 req/sec per IP), covers all endpoints
- **Auth logging**: Failed auth attempts logged with client IP for fail2ban integration (`[AUTH] ip=... result=...`). fail2ban jail defined inline in NixOS deployer config (not shipped here).
- **Metrics endpoint**: `/api/v1/metrics` is unauthenticated. Block in reverse proxy for production (only expose to internal monitoring network)

## CLI Tool

`oubot-cli` -- management CLI, available in the devShell (`nix develop -c oubot-cli ...`) or via `nix build .#cli`. Subcommands: `init`, `me`, `token`, `ntfy`, `language`, `pause`, `unpause`, `settings`, `admin`. Uses env vars `OUBOT_SERVER` and `OUBOT_TOKEN`.

## Key Dependencies

- **rocket** - Async web framework; the HTTP/2-capable development revision is pinned in `Cargo.toml`.
- **diesel / diesel-async** - PostgreSQL ORM and async queries, with the upstream `bb8` pool integration. Versions are in `Cargo.toml`.
- **governor** - IP-based rate limiting with DashMap
- **fluent-templates** - i18n (locales in `locales/`)

## ESP32-C3 Client

`clients/esp32/` — Rust no_std firmware (esp-hal 1.0.0 + esp-rtos + reqwless). Sends `GET /api/v1/up` heartbeats every 7s with bearer auth. WiFi reconnect, exponential backoff, LED feedback. Board: ESP32-C3 (RISC-V), GPIO8 LED (active-low).

Build/flash: see `docs/usage/esp32-c3.md`. Nix build: `nix build .#esp32-client --impure` (requires all 4 `OUBOT_*` env vars). DevShell uses `build-std` (nightly); Nix build uses fenix pre-built `rust-std` instead.

See `docs/specs/esp32-security-metrics.md` for the full design spec. See `docs/SETUP.md` for server deployment.

## Pico W Client

`clients/pico-w/` — Rust no_std firmware (embassy-rp 0.10 + cyw43 0.7 + reqwless 0.14). Same heartbeat/backoff/halt patterns as the ESP32 client. Board: Pico W (RP2040 Cortex-M0+), LED on CYW43439 WiFi chip (active-high).

Build/flash: see `docs/usage/pico-w.md`. Nix build: `nix build .#pico-w-client --impure` (requires all 4 `OUBOT_*` env vars). DevShell uses `build-std` (nightly); Nix build uses fenix pre-built `rust-std` instead. CYW43 firmware blobs (~231KB) in `clients/pico-w/firmware/` (Cypress Permissive Binary License).
