EVT_PY_PING = "ping"


def py_send(bus):
    bus.append(EVT_PY_PING)


def py_log(lines):
    # only passes the constant on: it handles nothing
    lines.append(str(EVT_PY_PING))


def py_is_ping(kind):
    return kind == EVT_PY_PING


def py_is_known(kind):
    return kind in (EVT_PY_PING, "pong")
