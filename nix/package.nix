{
  lib,
  stdenv,
  rustPlatform,
  cmake,
  mold,
  src,
}:

let
  # The flake passes `src = self`; use path literals for lib.fileset because
  # flake `self.outPath` is string-like and filesets require paths.
  cargoToml = lib.importTOML ../Cargo.toml;
  source = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../build.rs
      ../src
      ../prompts
      ../plugins
    ];
  };
in
rustPlatform.buildRustPackage {
  pname = "dirge";
  version = cargoToml.package.version;

  src = source;

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    cmake
    # evil-janet generates bindings during the build; bindgenHook also
    # provides clang on PATH for .cargo/config.toml's linker setting.
    rustPlatform.bindgenHook
  ]
  ++ lib.optionals stdenv.isLinux [ mold ];

  # Tests reach network/LLM providers and can exceed build timeouts.
  doCheck = false;

  meta = {
    description = "Minimal, fast pure-Rust coding agent with persistent memory";
    homepage = "https://github.com/dirge-code/dirge";
    license = lib.licenses.gpl3Only;
    mainProgram = "dirge";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ];
  };
}
