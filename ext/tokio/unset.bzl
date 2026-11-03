"""What `//ext/tokio:tokio` resolves to until a consumer sets it."""

def _unset_impl(ctx):
    fail(
        "@frustrate//ext/tokio:frustrate_tokio is compiled against your own tokio, and " +
        "--@frustrate//ext/tokio:tokio is not set.\n" +
        "Point it at the crate hub your bridge takes tokio from, in your .bazelrc:\n\n" +
        "    common --@frustrate//ext/tokio:tokio=@my_crates//:tokio\n",
    )

unset = rule(implementation = _unset_impl)
