package store

// Store keeps messages in memory.
type Store struct {
	messages []string
}

// New returns an empty store.
func New() *Store {
	return &Store{}
}

// Save appends a message.
func (s *Store) Save(message string) {
	s.messages = append(s.messages, message)
}

// Len is the number of saved messages.
func (s *Store) Len() int {
	return len(s.messages)
}
