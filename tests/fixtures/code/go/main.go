package main

import "example.com/chat/internal/server"

func main() {
	srv := server.Start()
	srv.Handle("hello")
}
