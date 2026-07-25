{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
}:

let
  version = "0.19.19";
  sel =
    {
      "x86_64-linux" = {
        triple = "x86_64-unknown-linux-gnu";
        hash = "sha256-UR80H4HdhRGsqzG/heUjAE97qEecjrkc+r2jxp9kGhQ=";
      };
      "aarch64-darwin" = {
        triple = "aarch64-apple-darwin";
        hash = "sha256-yG4JgugJjqO67CV05CfwrUFRg43B1HM6xWH7KbuJpyY=";
      };
    }
    .${stdenv.hostPlatform.system};
in
stdenv.mkDerivation {
  pname = "dirge-bin";
  inherit version;

  src = fetchurl {
    url = "https://github.com/dirge-code/dirge/releases/download/v${version}/dirge-${sel.triple}.tar.gz";
    inherit (sel) hash;
  };

  nativeBuildInputs = lib.optionals stdenv.isLinux [ autoPatchelfHook ];

  buildInputs = lib.optionals stdenv.isLinux [ stdenv.cc.cc.lib ];

  dontBuild = true;
  sourceRoot = ".";

  installPhase = ''
    runHook preInstall

    install -Dm755 dirge "$out/bin/dirge"

    runHook postInstall
  '';

  meta = {
    description = "Minimal, fast pure-Rust coding agent with persistent memory";
    homepage = "https://github.com/dirge-code/dirge";
    license = lib.licenses.gpl3Only;
    mainProgram = "dirge";
    platforms = [
      "x86_64-linux"
      "aarch64-darwin"
    ];
  };
}
