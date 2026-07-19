#[cfg(test)]
mod checker_tests;
#[cfg(test)]
mod edit_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod learning_loop_tests;
#[cfg(test)]
mod picker_tests;
#[cfg(all(test, feature = "semantic"))]
mod semantic_tests;
use ctor::ctor;
// Install rustls ring crypto provider before any test runs.
// Tests bypass main(), so the provider install in main() is not
// reached by #[cfg(test)] code paths.
#[ctor(unsafe)]
fn ensure_rustls_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
}
