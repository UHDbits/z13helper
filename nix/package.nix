{
  lib,
  rustPlatform,
  pkg-config,
  glib,
  gtk4,
  libadwaita,
  cairo,
  pango,
  gdk-pixbuf,
  graphene,
  libbpf,
  libxkbcommon,
  gtk4-layer-shell,
  libx11,
  libxcursor,
  libxrandr,
  libxi,
  wrapGAppsHook4,
  gsettings-desktop-schemas,
  withLayerShell ? false,
  buildType ? "release",
}:

rustPlatform.buildRustPackage {
  pname = "z13helper";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      let
        base = baseNameOf path;
      in
      base != ".git"
      && base != ".github"
      && base != "target"
      && base != ".cargo"
      && base != ".rustup"
      && base != "result"
      && base != "flake.nix"
      && base != "flake.lock"
      && !(lib.hasPrefix "nix" base && type == "directory" && base == "nix");
  };

  cargoLock.lockFile = ../Cargo.lock;

  inherit buildType;

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    glib # glib-compile-resources (build.rs)
  ];

  buildInputs = [
    glib
    gtk4
    libadwaita
    cairo
    pango
    gdk-pixbuf
    graphene
    libbpf
    libxkbcommon
    libx11
    libxcursor
    libxrandr
    libxi
    gsettings-desktop-schemas
  ]
  ++ lib.optional withLayerShell gtk4-layer-shell;

  # Match `make build`: ship GUI, daemon, and CLI only.
  cargoBuildFlags = [
    "-p"
    "z13helper"
    "-p"
    "z13helperd"
    "-p"
    "z13helperctl"
  ]
  ++ lib.optionals withLayerShell [
    "--features"
    "layer-shell"
  ];

  # Tests run as flake checks instead of during every package build.
  doCheck = false;

  # Only the GTK GUI needs the GApps wrapper; leave daemon/CLI unwrapped.
  dontWrapGApps = true;

  postInstall = ''
    mkdir -p $out/libexec
    mv $out/bin/z13helperd $out/libexec/z13helperd

    install -Dm644 contrib/com.ashtonantila.z13helper.desktop \
      $out/share/applications/com.ashtonantila.z13helper.desktop
    install -Dm644 assets/z13helper.svg \
      $out/share/icons/hicolor/scalable/apps/z13helper.svg
  '';

  postFixup = ''
    wrapProgram "$out/bin/z13helper" "''${gappsWrapperArgs[@]}"
  '';

  meta = {
    description = "GTK4 control platform for the ASUS ROG Flow Z13 (GZ302EA)";
    homepage = "https://github.com/UHDbits/z13helper";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "z13helper";
  };
}
