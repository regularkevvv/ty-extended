# Companion CLI

Generated from the companion's public `check --help`. Use `ty-extended server` for the editor server and `ty-extended --version` for the companion version.

```text
Check Python with official ty and configured Monty plugins

Usage: ty-extended check [OPTIONS] [PATH]...

  --fix                          Apply safe fixes to original source files
  --add-ignore                   Add ty: ignore comments to original source files
  -W, --watch                    Recheck when project or dependency files change
  --project PATH                 Select the project directory
  --python PATH                  Python interpreter or environment
  --python-version VERSION       Target Python version
  --python-platform PLATFORM     Target platform
  --typeshed PATH                Custom standard-library stubs
  --extra-search-path PATH       Additional import path (repeatable)
  -c, --config KEY=VALUE          Override TOML configuration (repeatable)
  --config-file PATH             Use a ty.toml file
  --error RULE                   Set a rule to error (repeatable)
  --warn RULE                    Set a rule to warning (repeatable)
  --ignore RULE                  Disable a rule (repeatable)
  --exclude PATTERN              Exclude source paths (repeatable)
  --respect-ignore-files         Respect ignore files (default)
  --no-respect-ignore-files      Ignore ignore files
  --force-exclude                Exclude explicitly selected paths too
  --no-force-exclude             Explicit paths bypass exclusions (default)
  --exclude-scripts              Exclude PEP 723 scripts
  --include-scripts              Include PEP 723 scripts
  --output-format FORMAT         full, concise, gitlab, github, junit
  --error-on-warning[=BOOL]       Fail on warnings (default: true)
  --exit-zero-on-warning         Fail only on errors
  --exit-zero                    Succeed even when diagnostics occur
  --color WHEN                   auto, always, never
  -q, --quiet                    Hide the success summary; -qq hides diagnostics
  -v, --verbose                  Print timing information
  --no-progress                 Suppress progress output
  --ty PATH                      Official ty executable (default: TY_COMPANION_TY or ty)
  -h, --help                     Show this help

Plugin configuration is discovered from ty.toml or [tool.ty] in pyproject.toml.
Temporary analysis files are removed when checking finishes.
```
