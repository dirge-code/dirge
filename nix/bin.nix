{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
}:

let
  version = "0.25.1";
  sel =
    {
      "x86_64-linux" = {
        triple = "x86_64-unknown-linux-gnu";
        hash = "sha256-N9ZM4i4KWZf/411gcG9Es/jNk5Sh0wcGA42A3eNS51o=";
      };
      "aarch64-darwin" = {
        triple = "aarch64-apple-darwin";
        hash = "sha256-C3ubB/BTgLrvPi3mOK7ijYEjDR/Zh2Lvcy8POKMLW0E=";
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
