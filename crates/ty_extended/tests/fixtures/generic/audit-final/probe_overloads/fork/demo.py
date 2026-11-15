from typing import Callable, overload, reveal_type
@overload
def echo(value: list[int]) -> object: ...
@overload
def echo(value: list[str]) -> object: ...
def echo(value: object) -> object:
    return value
def example() -> None:
    reveal_type(echo([]))
    reveal_type(echo([1]))
