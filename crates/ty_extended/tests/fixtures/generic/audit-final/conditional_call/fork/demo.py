from typing import Callable, assert_type
class Resource:
    status: str | None = None
    def activate(self) -> None:
        self.status = "ready"
    def release(self) -> None:
        self.status = None

def example(flag: bool) -> str:
    resource = Resource()
    flag and resource.activate()
    return resource.status
