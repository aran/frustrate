//! What generated glue calls. The modules live beside lib.rs; they are
//! declared here so that their public path is `frustrate::__private`.

#[path = "envelope.rs"]
pub mod envelope;
#[path = "executor.rs"]
pub mod executor;
#[path = "handle.rs"]
pub mod handle;
#[path = "pool.rs"]
pub mod pool;
#[path = "post.rs"]
pub mod post;
