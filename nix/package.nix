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
  # tree-sitter-mojo isn't on crates.io, so Cargo.lock carries it as a git
  # dependency and importCargoLock needs its fetch hash pinned here. The
  # hash was NAR-serialized offline (no Nix on the dev box); if it's ever
  # wrong the build error names the correct one — update it from there.
  cargoLock.outputHashes = {
    "tree-sitter-mojo-0.25.0" = "sha256-nw5dCRuVis8iWQ32qDhNPEqtN7konXaV/H2Om7+OjNU=";
  };

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
