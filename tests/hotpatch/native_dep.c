// A C archive the fixture library links, as a bridge does when it wraps a C
// library: the library's link then carries the archive, and a patch's must not.
int hotpatch_native_dep(int x) { return x + 1; }
