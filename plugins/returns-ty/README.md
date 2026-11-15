# returns-ty

[dry-python/returns](https://github.com/dry-python/returns) support for ty-extended.

Ports the behavior of `returns.contrib.mypy.returns_plugin` to ty's plugin
protocol: typed `curry`, `partial`, `flow`, `pipe`, `do` notation and `KindN`
higher-kinded type emulation.

## Usage

```console
$ pip install returns-ty
```

Enable plugin auto-discovery (or register the plugin explicitly), then run:

```console
$ tyx check
```

The plugin is discovered automatically from `returns_ty/ty-plugin.json` inside
the environment's site-packages.
