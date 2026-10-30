def mail_invoice(order):
    return render_invoice(order)


def render_invoice(order):
    return format_total(order)


def render_receipt(order):
    return format_total(order)


def format_total(order):
    return audit(order)
