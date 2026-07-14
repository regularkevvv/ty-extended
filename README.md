# ty-extended

[![PyPI](https://img.shields.io/pypi/v/ty-extended.svg)](https://pypi.org/project/ty-extended/)

A fork of ty with semantic extension support for framework-aware type checking.

<br />

<p align="center">
  <img alt="Shows a bar chart with benchmark results." width="500px" src="https://raw.githubusercontent.com/astral-sh/ty/main/docs/assets/ty-benchmark-cli.svg">
</p>

<p align="center">
  <i>Type checking the <a href="https://github.com/home-assistant/core">home-assistant</a> project without caching.</i>
</p>

<br />

ty-extended builds on [Astral's ty](https://github.com/astral-sh/ty) and keeps the command-line
executable named `ty`. The fork adds a semantic extension protocol, a Rust SDK for extension
authors, and live WASM extension execution.

ty-extended tracks ty's beta checker and language server while the extension protocol stabilizes.

## Highlights

- 10x - 100x faster than mypy and Pyright
- Comprehensive [diagnostics](https://docs.astral.sh/ty/features/diagnostics/) with rich contextual information
- Configurable [rule levels](https://docs.astral.sh/ty/rules/), [per-file overrides](https://docs.astral.sh/ty/reference/configuration/#overrides), [suppression comments](https://docs.astral.sh/ty/suppression/), and first-class project support
- Designed for adoption, with support for [redeclarations](https://docs.astral.sh/ty/features/type-system/#redeclarations) and [partially typed code](https://docs.astral.sh/ty/features/type-system/#gradual-guarantee)
- [Language server](https://docs.astral.sh/ty/features/language-server/) with code navigation, completions, code actions, auto-import, inlay hints, on-hover help, etc.
- Fine-grained [incremental analysis](https://docs.astral.sh/ty/features/language-server/#fine-grained-incrementality) designed for fast updates when editing files in an IDE
- Editor integrations for [VS Code](https://docs.astral.sh/ty/editors/#vs-code), [PyCharm](https://docs.astral.sh/ty/editors/#pycharm), [Neovim](https://docs.astral.sh/ty/editors/#neovim) and more
- Advanced typing features like first-class [intersection types](https://docs.astral.sh/ty/features/type-system/#intersection-types), advanced [type narrowing](https://docs.astral.sh/ty/features/type-system/#top-and-bottom-materializations), and
    [sophisticated reachability analysis](https://docs.astral.sh/ty/features/type-system/#reachability-based-on-types)

## Getting started

Run ty with [uvx](https://docs.astral.sh/uv/guides/tools/#running-tools) to get started quickly:

```shell
uvx --from ty-extended ty check
```

Or, check out the [ty playground](https://play.ty.dev) to try it out in your browser.

To learn more about using ty-extended, see the [documentation](./docs/index.md).

## Extension SDK

ty-extended also publishes the Rust crates extension authors use to build semantic extensions:

- [`ty_plugin_protocol`](https://crates.io/crates/ty_plugin_protocol): the stable JSON wire
    protocol for manifests, requests, responses, claims, and patches.
- [`ty_plugin_sdk`](https://crates.io/crates/ty_plugin_sdk): the author-facing SDK with
    `ManifestBuilder`, the `Plugin` trait, typed DSL helpers, JSON dispatch, and WASM exports.

Start with the [extension authoring guide](./docs/extension-authoring.md) for a working crate
layout, manifest claims, hook methods, and packaging guidance.

## Installation

To install ty-extended, see the [installation](./docs/installation.md) documentation.

To add the ty language server to your editor, see the [editor integration](https://docs.astral.sh/ty/editors/) guide.

## Getting help

If you have questions or want to report a bug, please open an
[issue](https://github.com/regularkevvv/ty-extended/issues) in this repository.

## Contributing

Most of the implementation lives in the `ruff` submodule, which points at
[regularkevvv/ruff-extended](https://github.com/regularkevvv/ruff-extended) for this fork.

See the
[contributing guide](./CONTRIBUTING.md) for more details.

## Version policy

ty-extended uses SemVer-compatible fork versioning that records the upstream ty base:

- upstream `0.0.58` maps to the initial `ty-extended 0.58.0` release;
- later fork releases on the same upstream base increment the patch, such as `0.58.1` and `0.58.2`;
- upstream `0.1.50` maps to `ty-extended 0.150.0`;
- once upstream reaches `1.0.0`, ty-extended follows that shape directly as `1.0.x`.

The semantic extension protocol and SDK crates are versioned independently. They are pre-1.0;
breaking changes may occur between any two `0.0.x` releases.

## FAQ

<!-- We intentionally use smaller headings for the FAQ items -->

<!-- markdownlint-disable MD001 -->

#### Which Python versions does ty support?

ty officially supports type checking code that targets Python 3.10 and later. Earlier versions
(Python 3.7 through 3.9) can still be selected, but may result in false negatives or false
positives due to a lack of bundled standard library stubs.

The target version is independent of the Python version used to install ty. For example, ty
installed from PyPI using Python 3.8 or later can type check code targeting Python 3.7; the
standalone installer does not require Python at all.

#### Why is ty doing \_\_\_\_\_?

See our [typing FAQ](https://docs.astral.sh/ty/reference/typing-faq).

#### How do you pronounce ty?

It's pronounced as "tee - why" ([`/tiː waɪ/`](https://en.wikipedia.org/wiki/Help:IPA/English#Key))

#### How should I stylize ty?

Just "ty", please.

<!-- markdownlint-enable MD001 -->

## License

ty is licensed under the MIT license ([LICENSE](LICENSE) or
<https://opensource.org/licenses/MIT>).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in ty
by you, as defined in the MIT license, shall be licensed as above, without any additional terms or
conditions.
