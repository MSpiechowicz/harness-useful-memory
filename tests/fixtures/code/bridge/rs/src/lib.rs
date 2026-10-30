pub const EVT_RS_PING: u8 = 1;

pub fn rs_send(bus: &mut Vec<u8>) {
    bus.push(EVT_RS_PING);
}

// only passes the constant on: it handles nothing
pub fn rs_log() -> String {
    format!("{}", EVT_RS_PING)
}

pub fn rs_handle(kind: u8) -> bool {
    match kind {
        EVT_RS_PING => true,
        _ => false,
    }
}

pub fn rs_is_ping(kind: u8) -> bool {
    kind == EVT_RS_PING
}
