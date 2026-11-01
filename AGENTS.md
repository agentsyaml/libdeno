# Development workflow

- Create and switch to a `feat/<xxx-feat-name>` branch before starting development work.
- Use a descriptive, kebab-case feature name.
- Push changes to the feature branch and open a GitHub Pull Request.
- Merge into the default branch only through a reviewed Pull Request after required CI checks pass.
- Do not commit or push development changes directly to the default branch.
- Preserve existing uncommitted changes when switching branches; never discard them to satisfy this workflow.
- Do not commit without being asked. Before any commit, the full CI gate must pass locally.

# Constraints

- Code, comments, and commit messages are English only. User-facing docs may be
  Chinese (`README.zh-CN.md`).
- Keep every source file under 500 lines. Generated files (lockfiles, snapshot
  tables) are exempt.
- Edit in small batches: roughly 500-2000 characters per change, not one giant
  rewrite. Sub-agents get the same rule, repeated in their brief.
- Do not run the full test suite or reformat after every change. Batch that at
  the end, or test a single file at a time.
- Never change system configuration or install global packages without explicit
  consent.
- Clean up scratch directories and temp artifacts after a large piece of work.

# Research and design

- Verify upstream behavior in its source, not only in docs or search results.
  This crate is pinned to specific Deno/V8 revisions; the source in the registry
  checkout is the authority.
- Check what already exists before adding anything. Prefer reusing or copying a
  proven upstream implementation over inventing one, then reduce it to the
  smallest thing that works.
- Stay critical of your own evidence. A passing test proves what it asserts, not
  what you assumed.

# Writing

- Docs should be dense and readable. Skip defensive framing ("this is not X, nor
  Y"); state what it is and why. Examples are fine for hard concepts.
- Explain without project shorthand. Assume a mid-level engineer who has never
  seen this codebase, and name things plainly instead of compressing them into
  internal labels.
