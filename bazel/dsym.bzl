"""The dSYM rustc writes beside a macOS shared library, carried through wrappers.

rules_rust offers it in the `dsym_folder` output group of a
`rust_shared_library`. A rule forwarding a library's files forwards that group
too, so `frustrate_code_asset` finds it behind any of them.
"""

visibility("//bazel/...")

def dsym_output_groups(library):
    """The `dsym_folder` output group of `library`, to return beside its files.

    Args:
      library: The Target whose library is forwarded.

    Returns:
      A list holding an `OutputGroupInfo` with `dsym_folder`, or empty.
    """
    if OutputGroupInfo in library and hasattr(library[OutputGroupInfo], "dsym_folder"):
        return [OutputGroupInfo(dsym_folder = library[OutputGroupInfo].dsym_folder)]
    return []

def library_dsym(library, file):
    """The dSYM of `file`, the shared library `library` provides, or None.

    Args:
      library: The Target providing `file`.
      file: The shared library File.

    Returns:
      The `<file>.dSYM` directory in `library`'s `dsym_folder` group, or None.
    """
    if OutputGroupInfo not in library or not hasattr(library[OutputGroupInfo], "dsym_folder"):
        return None
    for dsym in library[OutputGroupInfo].dsym_folder.to_list():
        if dsym.is_directory and dsym.basename == file.basename + ".dSYM":
            return dsym
    return None
