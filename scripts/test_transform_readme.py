"""Tests for the release README transformation."""

from __future__ import annotations

import unittest

from transform_readme import BENCHMARK_URL, BENCHMARK_URL_LIGHT, transform


class TransformReadmeTest(unittest.TestCase):
    def test_transforms_fork_links_without_requiring_benchmark(self) -> None:
        source = """\
# ty-extended

[Guide](./docs/extension-authoring.md)
[License](LICENSE)
[External](https://example.com/path)

<img src="./docs/assets/example.svg">

> [!IMPORTANT]
> Read this.

```mermaid
flowchart LR
    host --> wasm
```
"""

        result = transform(source, "0.59.1")

        self.assertIn(
            "https://github.com/regularkevvv/ty-extended/blob/v0.59.1/docs/extension-authoring.md",
            result,
        )
        self.assertIn(
            "https://github.com/regularkevvv/ty-extended/blob/v0.59.1/LICENSE",
            result,
        )
        self.assertIn("[External](https://example.com/path)", result)
        self.assertIn(
            'src="https://raw.githubusercontent.com/regularkevvv/ty-extended/v0.59.1/docs/assets/example.svg"',
            result,
        )
        self.assertIn("> IMPORTANT:", result)
        self.assertIn("```mermaid\nflowchart LR\n    host --> wasm\n```", result)

    def test_uses_light_benchmark_if_upstream_image_is_present(self) -> None:
        self.assertEqual(
            transform(BENCHMARK_URL, "0.59.1"),
            BENCHMARK_URL_LIGHT,
        )


if __name__ == "__main__":
    unittest.main()
