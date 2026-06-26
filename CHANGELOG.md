# Changelog

## Unreleased — fix/admin-theme-adherence

### Changed
- **AkurAI design-system theme adherence** (`src/landing.rs`)
  - All hardcoded hex colours removed from component CSS; every colour reference
    is now a `var(--token)` drawn from the AkurAI semantic token set
    (`--bg`, `--fg`, `--panel`, `--border`, `--accent`, `--accent-2`, `--ok`,
    `--warn`, `--info`, `--danger`, `--muted`, `--dim`, `--shadow`, `--radius`).
  - Added `themes_css()`: full 17-theme `:root[data-theme="…"]` token blocks,
    generated from the same base16 scheme YAMLs used by `akurai-css::theme` in
    the AkurAI-Framework, with the identical slot→token mapping. The first block
    (akurai dark) is also emitted as the bare `:root` default.
  - Added `flash_guard_js()`: synchronous `<script>` in `<head>` that reads the
    suite-wide `akurai-theme` cookie before first paint to eliminate FOUC.
  - Added `theme_picker_js()`: deferred `<script>` that mounts a `<select>`
    theme picker into any `[data-theme-picker]` slot, writes the `.olibuijr.com`
    domain cookie on change, syncs open tabs via `BroadcastChannel`, and
    re-adopts the cookie on `focus`/`visibilitychange` for cross-subdomain sync.
  - Admin page no longer uses a hardcoded light-cream background (`#f4f0e8`);
    it now inherits `var(--bg)` and `var(--panel)`, making it dark by default
    (AkurAI deep navy) and switchable to any suite theme alongside the landing
    page, akurai-drive, and the framework site.
  - `<html>` elements on all three pages (landing, admin, key-reveal) now carry
    `data-theme=""`, which the flash guard fills immediately.
  - `[data-theme-picker]` slots added to the landing nav and admin/key topbars.
