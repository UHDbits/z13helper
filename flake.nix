{
  description = "z13helper — GTK4 control platform for the ASUS ROG Flow Z13 (GZ302EA)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      packageOverlay = final: _prev: {
        z13helper = final.callPackage ./nix/package.nix { };
        z13helper-debug = final.callPackage ./nix/package.nix {
          buildType = "debug";
        };
        z13helper-layer-shell = final.callPackage ./nix/package.nix {
          withLayerShell = true;
        };
      };

      mkPkgs =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ packageOverlay ];
        };
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = mkPkgs system;
      in
      {
        packages = {
          default = pkgs.z13helper;
          z13helper = pkgs.z13helper;
          z13helper-debug = pkgs.z13helper-debug;
          z13helper-layer-shell = pkgs.z13helper-layer-shell;
        };

        apps = {
          default = {
            type = "app";
            program = "${pkgs.z13helper}/bin/z13helper";
          };
          z13helperctl = {
            type = "app";
            program = "${pkgs.z13helper}/bin/z13helperctl";
          };
        };

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.rustc
            pkgs.cargo
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.pkg-config
            pkgs.glib
            pkgs.gtk4
            pkgs.libadwaita
            pkgs.cairo
            pkgs.pango
            pkgs.gdk-pixbuf
            pkgs.graphene
            pkgs.libbpf
            pkgs.libxkbcommon
            pkgs.gtk4-layer-shell
            pkgs.libx11
            pkgs.libxcursor
            pkgs.libxrandr
            pkgs.libxi
            pkgs.gsettings-desktop-schemas
          ];

          shellHook = ''
            echo "z13helper dev shell (rustc $(rustc --version))"
            echo "  cargo build -p z13helper -p z13helperd -p z13helperctl"
            echo "  cargo test --workspace"
            echo "  cargo clippy --workspace --all-targets -- -D warnings"
            echo "  cargo fmt --all -- --check"
          '';
        };

        checks = {
          package = pkgs.z13helper;
          package-debug = pkgs.z13helper-debug;

          test = pkgs.z13helper.overrideAttrs (_old: {
            pname = "z13helper-test";
            doCheck = true;
            cargoTestFlags = [ "--workspace" ];
            # Nix build dirs make AF_UNIX paths exceed SUN_LEN; keep sockets short.
            preCheck = ''
              export TMPDIR=$(mktemp -d /tmp/z13t.XXXXXX)
              export HOME="$TMPDIR"
            '';
            installPhase = "mkdir -p $out && touch $out/ok";
            dontWrapGApps = true;
            postFixup = "";
          });

          clippy = pkgs.z13helper.overrideAttrs (old: {
            pname = "z13helper-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
              pkgs.clippy
              pkgs.rustc
              pkgs.cargo
            ];
            buildPhase = ''
              runHook preBuild
              cargo clippy -p z13helper -p z13helperd -p z13helperctl --all-targets -- -D warnings
              runHook postBuild
            '';
            installPhase = "mkdir -p $out && touch $out/ok";
            dontWrapGApps = true;
            doCheck = false;
            postFixup = "";
          });
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    )
    // {
      overlays.default = packageOverlay;

      nixosModules.default = self.nixosModules.z13helper;
      nixosModules.z13helper =
        {
          lib,
          pkgs,
          ...
        }:
        {
          imports = [ ./nix/nixos-module.nix ];
          services.z13helperd.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          programs.z13helper.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };

      homeModules.default = self.homeModules.z13helper;
      homeModules.z13helper =
        {
          lib,
          pkgs,
          ...
        }:
        {
          imports = [ ./nix/home-manager.nix ];
          programs.z13helper.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      homeManagerModules = self.homeModules;
    };
}
