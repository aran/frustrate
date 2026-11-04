pub mod api;

// Written by frustrate codegen (the frustrate_bridge action under Bazel).
mod frustrate_generated;

// The custom backend's symbol; nothing names it, so only this line links it.
use frustrate_getrandom as _;
