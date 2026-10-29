class Receipt:
    def __init__(self, total: int) -> None:
        self.total = total

    def describe(self) -> str:
        return f"total {self.total}"


class Invoice:
    """Same method name as Receipt, a different class."""

    def describe(self) -> str:
        return "invoice"
