export const EVT_TS_PING = "ping";

export function tsSend(bus: string[]) {
  bus.push(EVT_TS_PING);
}

// only passes the constant on: it handles nothing
export function tsLog(lines: string[]) {
  lines.push(String(EVT_TS_PING));
}

export function tsHandle(kind: string) {
  switch (kind) {
    case EVT_TS_PING:
      return true;
  }

  return false;
}

export function tsIsPing(kind: string) {
  return kind === EVT_TS_PING;
}
