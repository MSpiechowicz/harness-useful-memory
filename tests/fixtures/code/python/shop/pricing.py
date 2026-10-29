from .models import Receipt


def add_tax(net: int) -> int:
    return net + net * 23 // 100


def make_receipt(net: int) -> Receipt:
    return Receipt(add_tax(net))
