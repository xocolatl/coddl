# Git hooks

Version-controlled git hooks for the repo. Run once per clone:

```sh
sh tools/git-hooks/install.sh
```

This installs a `.git/hooks/pre-commit` shim that execs `tools/git-hooks/pre-commit`.
It does **not** set `core.hooksPath`, so any local hooks already in `.git/hooks`
(e.g. the ctags `post-*` hooks) keep working.

## `pre-commit` — formatting gate

- **Rust** — `cargo fmt --all --check` over the whole workspace. rustfmt's rules
  are fixed (stock defaults, no `rustfmt.toml`); keep the baseline clean and any
  drift fails the commit. Fix with `cargo fmt --all`.
- **Coddl** — `coddl fmt --check` on the **staged** `.cd` files only. Staged-only
  is deliberate: the Coddl formatter (`coddl-fmt`) is still maturing, so
  pre-existing `.cd` files are left alone until a deliberate `coddl fmt --write`
  sweep — you reformat only what you touch. When the formatter's rules change,
  that sweep is the expected churn. Fix with `coddl fmt --write <file>`.

Bypass a single commit with `git commit --no-verify`.
