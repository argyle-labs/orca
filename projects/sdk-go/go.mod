// Go port of the orca plugin SDK. Sibling of projects/sdk (the Rust
// reference). Targets the same wire contract — same framing, same JSON-RPC
// shapes, same manifest format, same conformance scenario.
//
// Module path is local-only for now; will be republished under a real
// VCS path once we publish.
module orca/sdk-go

go 1.26

require github.com/pelletier/go-toml/v2 v2.3.1
