{
  git,
  lib,
  makeWrapper,
  rustPlatform,
  worktrunk,
}: rustPlatform.buildRustPackage {
  pname = "lop";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: type:
      (lib.cleanSourceFilter path type)
      && baseNameOf path != "target";
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [makeWrapper];
  nativeCheckInputs = [
    git
    worktrunk
  ];

  postInstall = ''
    wrapProgram $out/bin/lop \
      --prefix PATH : ${lib.makeBinPath [git worktrunk]}
  '';

  meta = {
    description = "Conservative cleanup of stale Git worktrees";
    homepage = "https://github.com/johnallen3d/lop";
    license = lib.licenses.mit;
    mainProgram = "lop";
    platforms = lib.platforms.unix;
  };
}
