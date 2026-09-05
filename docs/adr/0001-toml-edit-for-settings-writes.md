# ADR-0001: Write settings with `toml_edit` to preserve the user's config file

- Status: accepted
- Date: 2026-09-05

## Context

The in-app settings panel writes a handful of keys (`[languages]`, `[ui]`,
`[hotkey]`, per-mode `thinking`) back to `config.toml`. That file is
hand-edited and heavily commented: the starter template and
`config.example.toml` document every key inline, and users keep prompt
overrides there. The existing `toml` dependency is built with
`default-features = false, features = ["parse"]` — it cannot serialize at all,
and serializing a `Config` struct would in any case drop every comment,
reorder tables, and emit keys the user never set.

## Decision

Add `toml_edit` (0.20, the version already pulled in transitively by `toml`
0.8) as a direct dependency and edit the document in place: parse the file
into a `toml_edit::Document`, set or remove only the keys the panel owns, and
write the result back atomically (temp file + rename). Everything else in the
file — comments, ordering, unrelated keys, the prompt blocks — stays
byte-identical.

## Consequences

- Comments and layout survive a GUI save; a user can keep editing the same
  file by hand.
- No new compiled code beyond what `toml` already links; the binary stays a
  single file with no runtime dependencies.
- The panel must know the TOML key path of each setting it edits (kept in one
  place, `settings::apply_patch`, with a test that comments are preserved).
- Settings the panel does not own (prompts, `[api]`, `[[models]]`) remain
  file-only; the panel links to "Open Config" for them.
