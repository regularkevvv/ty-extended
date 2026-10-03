from __future__ import annotations

from typing import reveal_type

import example

token = example.issue_token("input")
reveal_type(token)  # revealed: example.Token (from the Monty plugin)
