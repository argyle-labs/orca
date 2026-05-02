---
name: FRONTEND
description: Frontend conventions for the Orca web project (projects/frontend)
---

# Frontend Conventions

## Generated hooks over raw fetch

Always use generated hooks from `src/api/hooks.ts` (auto-generated from OpenAPI spec via `npm run gen`). Use hooks like `useGetTree`, `useListSpecs` in components instead of calling `client.*` directly or using `fetch`.

Raw fetch is only acceptable when a hook does not yet exist — in that case, run `orca gen` to regenerate first.

## Types from generated types

Types come from `src/api/types.ts`. Never define local interfaces that duplicate generated types.

## Thin client

The frontend is a thin client — all business logic on the server. No raw `fetch()`, no frontend parsing or normalization. Everything flows through the generated API client.
