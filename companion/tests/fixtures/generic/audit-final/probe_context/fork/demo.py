from typing import Callable, TypedDict, assert_type
class Row(TypedDict):
    value: int
def echo(value: list[int]) -> object:
    return value
def consume(function: Callable[[int], int], row: Row) -> int:
    return function(row["value"])
def example() -> None:
    assert_type(echo([]), list[int])
    assert_type(consume(lambda item: item + 1, {"value": 1}), int)
