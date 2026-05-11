// Go port of the orca plugin SDK. Sibling of projects/sdk (the Rust
// reference). Targets the same wire contract — same framing, same JSON-RPC
// shapes, same manifest format, same conformance scenario.
//
// Plugins fetch this module by version from GitHub. Tag releases on the
// orca repo as `projects/sdk-go/vX.Y.Z` (Go's submodule versioning
// convention). Consumers in private deployments must set GOPRIVATE so
// `go get` skips the public proxy and authenticates directly to GitHub.
module github.com/scottdkey/orca/projects/sdk-go

go 1.26

require github.com/pelletier/go-toml/v2 v2.3.1
