Closes #[issue number]

## Description

Describe in English the changes made in this pull request and the problem it solves.
Please keep **1 PR to solve 1 problem**, and keep **small improvements small** to make the PR easier to review and to merge.

## Screenshot

| Device | Before | After |
| ------ | ------ | ----- |
| [e.g. OpenHarmony 7.0 emulator] | [Put Before Screenshot here] | [Put After Screenshot here] |

## Public API

List every public item this pull request adds, changes or removes, with its signature and one line on what it is for: Rust items, ArkTS template APIs, CLI commands and flags, `nsh.json` keys. If none, remove this section.

## Breaking Changes

Describe any breaking changes introduced by this pull request. If none, remove this section.

```diff
- Old code snippet
+ New code snippet
```

## How to Test

Please describe the tests that you ran to verify your changes, and the device or emulator (OS version, API level) you ran them on. Provide instructions so we can reproduce.

## Checklist

- [ ] I have read the [CONTRIBUTING](../CONTRIBUTING.md) document and followed the guidelines.
- [ ] The PR title follows `<area>: <What the change does>` (no `fix:` / `feat:` prefixes).
- [ ] Reviewed the changes in this PR and confirmed AI generated code (if any) is accurate.
- [ ] Passed `cargo test --workspace`, `cargo fmt --all` and clippy (including the `aarch64-unknown-linux-ohos` runs).
- [ ] Ran the change on a HarmonyOS / OpenHarmony device or emulator (if it is visible or platform-specific).
