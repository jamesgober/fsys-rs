# Migration

> **Status — 0.6.0:** This file is a stub. Migration guides will be
> populated when fsys ships its first stable release (1.0.0). Until
> then, every minor version may carry breaking changes; consult the
> per-release `CHANGELOG.md` for breaking-change notes.

## What lives here at 1.0.0+

When 1.0.0 ships, this file becomes the canonical migration
reference. Each MAJOR version bump documents:

1. **Breaking changes** — every signature change, removed item,
   renamed type, or behavioral change visible to API consumers.
2. **Migration recipe** — concrete before/after code samples for
   every break.
3. **Deprecation timeline** — items marked `#[deprecated]` in the
   prior release, scheduled for removal in this one.
4. **Tooling** — `cargo fix --edition` equivalents for any
   mechanical migrations.

## Pre-1.0 breaking-change policy

Per Cargo's SemVer rules for `0.x.y`, **every minor version is a
potential breaking-change opportunity.** fsys uses this freedom
deliberately while the API surface is being finalised:

- 0.4.0 → 0.5.0 broke `DriveInfo::plp` (`bool` → `PlpStatus` enum).
- 0.5.0 → 0.5.1 was non-breaking (io_uring lift only).
- 0.5.1 → 0.6.0 added several new items but did not break existing
  signatures.

Each `CHANGELOG.md` `[X.Y.Z]` block carries a `### Migration`
subsection when changes are user-visible.

## What's locked already

The following commitments hold from 0.5.0 onwards and will
continue through 1.0:

- `Method` variant set: `Sync`, `Data`, `Mmap`, `Direct`,
  `Journal` (reserved 0.7.0), `Auto`. No new variants until 1.0.
- `Error` variant codes (`FS-XXXXX`) are stable. Codes never
  change for an existing variant; new variants get new codes.
- The `Handle` / `Builder` surface API names. Implementations
  may evolve.
- `fsys::primitive` constant *strings* (the values, not just the
  constant names). New primitives may be added.
