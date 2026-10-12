from typing import Callable
def decode(value: object) -> object:
    return value
def example(decode: Callable[[str], str]) -> int:
    return decode("42") + 1
