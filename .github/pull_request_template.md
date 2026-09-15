## Summary

<!-- Brief explanation of what this change does and why it was made. -->

## Related Issues

<!-- Link relevant issues here, e.g. "Closes #123" or "Fixes #45" -->

## Type of Change

- [ ] 🐛 Bug fix (non-breaking change which fixes an issue)
- [ ] ✨ New feature (non-breaking change which adds functionality)
- [ ] ⚡ Performance improvement
- [ ] 📝 Documentation update
- [ ] 🧪 Tests / CI improvement
- [ ] 🔨 Refactoring / internal cleanup

## Checklist

- [ ] My code adheres to the [Contributing Guide](CONTRIBUTING.md) and [Code of Conduct](CODE_OF_CONDUCT.md).
- [ ] **Coexistence safe**: Change does not fork or compromise shared `/var/lib/rpm` state.
- [ ] Code is formatted with `cargo fmt --all --check`.
- [ ] Linter passes with `cargo clippy --all-targets --all-features -- -D warnings`.
- [ ] All unit and integration tests pass with `cargo test --all --all-features`.
- [ ] Added test coverage for new functionality or bug fixes.
- [ ] Documentation updated if CLI syntax or configuration behavior changed.
