---
name: FRONTEND
description: Frontend conventions for the Orca web project (projects/frontend)
---

# Frontend Conventions

## hey-api SDK is the only API surface

All API access goes through the hey-api-generated TypeScript SDK in `projects/frontend/src/lib/sdk/`. The SDK is regenerated from the live OpenAPI 3.1 spec at `http://localhost:12000/api/openapi.json` via `npm run gen:client`. Every tool method is emitted automatically from `#[orca_tool]` annotations across the domain crates.

Raw `fetch()` is never acceptable in app code. If a tool isn't on the SDK, add a `#[orca_tool]`-annotated function in the relevant domain crate — the typed method appears on the SDK after `gen:client`.

## Auth

Cookie-session auth via `:12000`. Sign-in / sign-up pages set the session; subsequent SDK calls send credentials automatically (CORS in dev mirrors origin + allow-credentials).

## Types

Types come from the hey-api `.d.ts` files in `src/lib/sdk/`. Never define local interfaces that duplicate them.

## Thin client

The frontend is a thin client — all business logic on the server. No raw `fetch()`, no frontend parsing or normalization. Everything flows through the SDK.
