# AGENTS.md — Play

Play is a desktop app and CLI that plays HLS/HTTP URLs in VLC with seeking.
Stack: Tauri 2, SvelteKit, TypeScript, Tailwind CSS, Rust engine.

## Layout

- `src/` — SvelteKit UI
- `src-tauri/src/` — Rust engine (`urls`, `fetch`, `hls`, `proxy`, `capture`, `player`, `download`, `prefs`, `commands`, `cli`)
- `extension/` — Send to Play browser bridge (unpacked MV3)
- `docs/` — docs and assets

## Commands

- `npm install`
- `npm run tauri dev` — desktop app dev loop
- `npm run tauri build` — native bundle under `src-tauri/target/release/bundle/`
- `npm run check` — SvelteKit sync + `svelte-check`
- `cd src-tauri && cargo test` — Rust tests
- `cargo install --path src-tauri --bin play` — install `play` CLI

## Rules

- Do what was asked; nothing more, nothing less.
- Always read a file before editing it.
- Keep files under 500 lines.
- Validate input at system boundaries.
- Never create files unless necessary; prefer editing existing files.
- Never create documentation files unless explicitly requested.
- Never save working files or tests to the repo root; use `src/`, `tests/`, `docs/`, `config/`, or `scripts/`.
- Never commit secrets, credentials, `.env` files, or per-directory `.play.json` settings.
- Never leave real debug stream URLs or endpoint URLs in source code or tests; use placeholders such as `https://cdn.example/hls/master.txt`.
- Never auto-commit or auto-push; wait for an explicit request.
- Never add `Co-Authored-By` lines to commit messages.

## Verify before done

- Run tests after code changes: `cd src-tauri && cargo test`.
- Run `npm run check`, and `npm run build` for UI changes.
- For UI, layout, styling, routing, or rendered-data changes, verify in the browser end to end the way a user would (click, type, submit, navigate), including empty and error states and desktop plus mobile viewports when layout changed. Confirm shared state stays consistent across routes. If no browser is available, verify with the closest substitute and say what was not verified.
- Claim done, fixed, or tested only when tool output supports it.

## Claude Code + Ruflo

When working under Claude Code with Ruflo coordination, also follow `CLAUDE.md` for swarm topology, agent routing, memory, hooks, and CLI usage. Other agents can ignore that file.
