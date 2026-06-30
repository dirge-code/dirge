{
  lib,
  stdenv,
  mkShell,
  rustc,
  cargo,
  rustfmt,
  clippy,
  rust-analyzer,
  cmake,
  mold,
  clang,
  libclang,
  pkg-config,
}:

mkShell {
  packages = [
    rustc
    cargo
    rustfmt
    clippy
    rust-analyzer
    cmake
    clang
    pkg-config
  ]
  ++ lib.optionals stdenv.isLinux [ mold ];

  LIBCLANG_PATH = "${libclang.lib}/lib";
}
