## Summary

<!-- What does this change and why? -->

## Type of change

- [ ] 🧪 New runner or linter support
- [ ] 🎯 Verdict accuracy (false positive / false negative fix)
- [ ] 🐛 Bug fix
- [ ] ✨ New feature
- [ ] 📝 Documentation
- [ ] 🔧 Refactoring

## Testing

- [ ] `cargo test` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] Added a test that fails without this change

<!-- For a runner: paste the real passing and failing output your parser was built against. -->

## Design rules

<!-- These are what make the output trustworthy; see CONTRIBUTING.md. -->

- [ ] No model calls added — every verdict still derives from recorded evidence
- [ ] Ambiguous signals return `Inconclusive` rather than guessing
- [ ] Claims and evidence remain independent (neither path reads the other)
- [ ] Any new verdict shows the evidence it rests on
- [ ] Hook mode still degrades safely on bad input

## Related issues

<!-- Closes #123 -->
