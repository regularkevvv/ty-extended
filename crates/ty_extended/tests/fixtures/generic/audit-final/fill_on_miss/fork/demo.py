from typing import Callable, assert_type
class Resource:
    token: int
    status: str | None = None
    def activate(self) -> None:
        self.status = "ready"
    def release(self) -> None:
        self.status = None

def example(resource: Resource) -> bytes:
    resource = Resource()
    return resource.token
