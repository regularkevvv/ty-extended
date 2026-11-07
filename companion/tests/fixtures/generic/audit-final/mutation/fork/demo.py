from typing import Callable, assert_type
class Resource:
    def __setitem__(self, key: str, value: bytes) -> None:
        pass
    status: str | None = None
    def activate(self) -> None:
        self.status = "ready"
    def release(self) -> None:
        self.status = None

def example() -> None:
    resource = Resource()
    resource["token"] = b"changed"
