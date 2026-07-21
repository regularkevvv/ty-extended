# Changelog

ty-extended tracks upstream [ty](https://github.com/astral-sh/ty) and adds semantic extension
support on top of it. This file records what the fork changes.

Upstream's changes are not duplicated here. Every release below names the upstream ty version it is
built on and links to that release's notes.

## 0.61.0

Built on [ty 0.0.61](https://github.com/astral-sh/ty/releases/tag/0.0.61). Released 2026-07-21.

No breaking changes. `ty_plugin_protocol` and `ty_plugin_sdk` stay at `0.0.4`, and the wire
protocol stays at `0.3`, so extensions built against 0.60.0 continue to load unchanged.

### Plugin behaviour

- Upstream generalised a tuple's variable-length segment so it may be an unpacked `TypeVarTuple`,
    as in `tuple[*Ts]`. The protocol has no representation for a type variable, so a segment of
    that kind is now reported to extensions as a variable-length tuple whose element type is
    `object`. Reporting it as a fixed-length tuple instead would have told extensions the wrong
    arity.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.61.0,<0.62.0`.

## 0.60.0

Built on [ty 0.0.60](https://github.com/astral-sh/ty/releases/tag/0.0.60). Released 2026-07-21.

### Breaking changes

- The `ty_plugin_protocol` and `ty_plugin_sdk` crates are released as `0.0.4`.
    `PluginRequest::ValidateMutation` and `PluginResponse::Manifest` now hold their payloads in a
    `Box`, because each variant was several times larger than its siblings and every request and
    response paid for it. Extension authors must recompile, but **the wire format is unchanged** —
    `Box` is transparent to serde — so `CURRENT_PROTOCOL_VERSION` stays at `0.3` and manifests
    published against earlier SDK versions continue to negotiate successfully.

### Documentation

- The `plugins` configuration options now appear in the configuration reference and the JSON
    schema. They had been missing since the options were introduced.
- The extension authoring guide and both plugin crate READMEs showed an example
    `ty_compatibility` range of `>=0.59.0,<0.60.0`, which excludes this release; an extension
    following them would have been rejected during protocol negotiation. They now show
    `>=0.60.0,<0.61.0`.
- The standalone installer URLs in the installation guide had been stale since `0.58.2`.

## Earlier releases

These predate this changelog, so only their upstream base is recorded.

| ty-extended                                                                | Upstream base                                                    | Released   |
| -------------------------------------------------------------------------- | ---------------------------------------------------------------- | ---------- |
| [0.59.1](https://github.com/regularkevvv/ty-extended/releases/tag/v0.59.1) | [ty 0.0.59](https://github.com/astral-sh/ty/releases/tag/0.0.59) | 2026-07-14 |
| [0.59.0](https://github.com/regularkevvv/ty-extended/releases/tag/v0.59.0) | [ty 0.0.59](https://github.com/astral-sh/ty/releases/tag/0.0.59) | 2026-07-14 |
| [0.58.2](https://github.com/regularkevvv/ty-extended/releases/tag/v0.58.2) | [ty 0.0.58](https://github.com/astral-sh/ty/releases/tag/0.0.58) | 2026-07-14 |
| [0.58.1](https://github.com/regularkevvv/ty-extended/releases/tag/v0.58.1) | [ty 0.0.58](https://github.com/astral-sh/ty/releases/tag/0.0.58) | 2026-07-11 |
| [0.58.0](https://github.com/regularkevvv/ty-extended/releases/tag/v0.58.0) | [ty 0.0.58](https://github.com/astral-sh/ty/releases/tag/0.0.58) | 2026-07-10 |
