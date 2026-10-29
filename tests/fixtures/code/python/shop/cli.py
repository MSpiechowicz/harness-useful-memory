from shop.pricing import make_receipt


def run(net: int) -> str:
    receipt = make_receipt(net)
    return receipt.describe()
