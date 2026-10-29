package server

import (
	"example.com/chat/internal/events"
	"example.com/chat/internal/store"
)

// Server accepts messages.
type Server struct {
	store  *store.Store
	events chan events.Kind
}

// Start builds a server over a fresh store.
func Start() *Server {
	return &Server{store: store.New(), events: make(chan events.Kind, 8)}
}

// Handle saves one message and announces it.
func (s *Server) Handle(message string) {
	s.store.Save(message)
	s.events <- events.EvtNew
}

// Run reacts to the announcements.
func (s *Server) Run() {
	for kind := range s.events {
		switch kind {
		case events.EvtNew:
			s.store.Len()
		case events.EvtDone:
			return
		}
	}
}
