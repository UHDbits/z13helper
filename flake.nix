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
      cargoMetadata = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      cargoVersion = cargoMetadata.workspace.package.version;

      packageOverlay = final: _prev: {
        z13helper = final.callPackage ./nix/package.nix { };
        z13helper-debug = final.callPackage ./nix/package.nix {
          buildType = "debug";
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
        bpfClang = pkgs.llvmPackages.clang-unwrapped;
        bpfLlvm = pkgs.llvmPackages.llvm;
        bpfLibbpf = pkgs.lib.getDev pkgs.libbpf;
        bpfCflags = "-O2 -g -target bpf -D__TARGET_ARCH_x86";
        bpfIncludeFlags = "-I${bpfLibbpf}/include -I${pkgs.linuxHeaders}/include";
      in
      {
        packages = {
          default = pkgs.z13helper;
          z13helper = pkgs.z13helper;
          z13helper-debug = pkgs.z13helper-debug;
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
            bpfClang
            bpfLlvm
            pkgs.linuxHeaders
          ];

          BPF_CLANG = "${bpfClang}/bin/clang";
          BPF_STRIP = "${bpfLlvm}/bin/llvm-strip";
          BPF_CFLAGS = bpfCflags;
          BPF_INCLUDE_FLAGS = bpfIncludeFlags;

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
              cargo clippy --workspace --all-targets --locked -- -D warnings
              runHook postBuild
            '';
            installPhase = "mkdir -p $out && touch $out/ok";
            dontWrapGApps = true;
            doCheck = false;
            postFixup = "";
          });

          fmt =
            pkgs.runCommand "z13helper-fmt"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                ];
              }
              ''
                cd ${self}
                cargo fmt --all -- --check
                touch "$out"
              '';

          metadata = pkgs.runCommand "z13helper-metadata-parity" { } ''
            test "${pkgs.z13helper.version}" = "${cargoVersion}"
            test "${cargoMetadata.workspace.package.license}" = "MIT"
            touch "$out"
          '';

          bpf =
            pkgs.runCommand "z13helper-bpf-source-object"
              {
                nativeBuildInputs = [
                  bpfClang
                  bpfLlvm
                  bpfLibbpf
                  pkgs.linuxHeaders
                  pkgs.gnumake
                ];
              }
              ''
                cd ${self}
                make check-bpf \
                  TARGET_DIR="$TMPDIR/target" \
                  BPF_CLANG="${bpfClang}/bin/clang" \
                  BPF_STRIP="${bpfLlvm}/bin/llvm-strip" \
                  BPF_CFLAGS="${bpfCflags}" \
                  BPF_INCLUDE_FLAGS="${bpfIncludeFlags}"
                touch "$out"
              '';
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
          services.z13helperd.package =
            lib.mkDefault
              self.packages.${pkgs.stdenv.hostPlatform.system}.default;
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
