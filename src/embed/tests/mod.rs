//! Tests of the embedder with a fake engine (vectors from hashed words, different per model id),
//! real SQLite stores, a local HTTP server for the downloads and real sockets. The real model has
//! its own `#[ignore]`d test.

mod download;
mod real;
mod service;
mod socket;
mod support;
mod switch;
