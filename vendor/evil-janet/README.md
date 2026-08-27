# evil-janet

This is a rust crate providing low level bindings to the [janet](https://janet-lang.org/) C API.

# Versioning scheme

The package major and minor versions follow the janet releases the
bundled headers are compatible with. The patch version may, but does
not necessarily match the janet release.

# Feature flags

-   **link-amalg:** The crate will link the compiled Janet amalgamation
-   **link-system:** Link to the libjanet library from the system (not recommended generally)
-   **system:** Use Janet header from the system
-   **debug-symbols:** Compile with max debug symbols

if the feature link-amalg is enabled, the crate will link the compiled
janet amalgamation, otherwise this crate is header only, which is useful
for building standalone janet modules.

# Environment variables

**These variables are only used when using the feature `link-amalg`**

This crate uses environment variables to overwrite some Janet definitions.

-   `JANET_RECURSION_GUARD=<integer>`
-   `JANET_MAX_PROTO_DEPTH=<integer>`
-   `JANET_MAX_MACRO_EXPAND=<integer>`
-   `JANET_STACK_MAX=<integer>`

# Safety

This crate makes no attempt memory safety.
It is the bare minimum required to use the Janet API from
Rust. For a higher level crate you can look for [JanetRS](https://github.com/GrayJack/janetrs).
