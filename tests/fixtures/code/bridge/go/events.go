package bridge

// EvtGoPing is sent by GoSend and handled by GoHandle and GoIsPing.
const EvtGoPing = "ping"

// GoSend sets the event.
func GoSend(box *Box) {
	box.last = EvtGoPing
}

// GoLog only passes the constant on as an argument: it handles nothing.
func GoLog(sink []string) []string {
	return append(sink, EvtGoPing)
}

// GoHandle switches on the event.
func GoHandle(kind string) bool {
	switch kind {
	case EvtGoPing:
		return true
	}

	return false
}

// GoIsPing compares with the event.
func GoIsPing(kind string) bool {
	return kind == EvtGoPing
}

// Box keeps the last event.
type Box struct {
	last string
}
