package events

// Kind names what happened.
type Kind int

const (
	// EvtNew is sent when a message arrives.
	EvtNew Kind = iota
	// EvtDone is sent when the server stops.
	EvtDone
)
