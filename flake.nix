{
  description = "topbar - GNOME Shell-style top bar for Niri (GTK4 + layer-shell)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    crane.url = "github:ipetkov/crane";
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      git-hooks,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = pkgs.lib;
      craneLib = crane.mkLib pkgs;

      # crane's filter keeps Rust and TOML sources only. Three other things are
      # compiled in with include_str!/include_bytes! and have to survive it: the
      # example config, which is the binary's --print-example-config output, the
      # recorded protocol fixtures the parser tests are built on, and the crypto
      # widget's logos.
      src = lib.cleanSourceWith {
        src = ./.;
        filter =
          path: type:
          (craneLib.filterCargoSources path type)
          || (builtins.baseNameOf path == "config.toml")
          || (lib.hasInfix "/tests/fixtures/" path)
          || (lib.hasInfix "/assets/crypto/" path);
      };

      commonArgs = {
        inherit src;
        pname = "topbar";
        version = "2.0.0";
        strictDeps = true;
        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
          # `dbus-daemon`, for the notification daemon's bus tests. They stand
          # up a private bus per test rather than touching the session's, so
          # the tool has to be on PATH at *test* time, which strictDeps makes
          # a nativeBuildInputs job rather than a buildInputs one.
          dbus
          # `libspa-sys` generates the PipeWire bindings with bindgen, which
          # needs libclang *and* a C standard library it can find. The hook
          # sets both LIBCLANG_PATH and BINDGEN_EXTRA_CLANG_ARGS; with only
          # the first, clang finds its own `inttypes.h` and then fails to
          # resolve the `#include_next <inttypes.h>` inside it.
          rustPlatform.bindgenHook
        ];
        buildInputs = with pkgs; [
          gtk4
          gtk4-layer-shell
          glib
          pango
          gdk-pixbuf
          librsvg
          cairo
          graphene
          wayland
          dbus
          udev
          libpulseaudio
          pipewire
          adwaita-icon-theme
          hicolor-icon-theme
          gsettings-desktop-schemas
        ];
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      topbar = craneLib.buildPackage (
        commonArgs
        // {
          inherit cargoArtifacts;
          cargoExtraArgs = "-p topbar";
          # wrapGAppsHook4 does not carry buildInputs' icon themes into the
          # wrapper, so a host with no themes of its own was left with GTK's
          # embedded subset — most names resolved, network-vpn and
          # power-profile-* did not. Ship them explicitly: the panel renders
          # identically on a bare machine and under a fully themed session.
          preFixup = ''
            gappsWrapperArgs+=(
              --prefix XDG_DATA_DIRS : "${pkgs.adwaita-icon-theme}/share:${pkgs.hicolor-icon-theme}/share"
            )
          '';
          meta = {
            description = "GNOME Shell-inspired GTK4 top bar for Niri";
            license = lib.licenses.mit;
            mainProgram = "topbar";
            platforms = [ "x86_64-linux" ];
          };
        }
      );

      pre-commit = git-hooks.lib.${system}.run {
        src = ./.;
        hooks = {
          rustfmt.enable = true;
          nixfmt-rfc-style.enable = true;
          convco.enable = true;
          end-of-file-fixer.enable = true;
          trim-trailing-whitespace.enable = true;
        };
      };
    in
    {
      packages.${system} = {
        default = topbar;
        inherit topbar;
      };

      overlays.default = final: prev: {
        topbar = self.packages.${final.stdenv.hostPlatform.system}.default;
      };

      apps.${system}.default = {
        type = "app";
        program = lib.getExe topbar;
      };

      checks.${system} = {
        build = topbar;
        fmt = craneLib.cargoFmt {
          inherit src;
          pname = "topbar";
          version = "2.0.0";
        };
        clippy = craneLib.cargoClippy (
          commonArgs
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--workspace --all-targets -- -D warnings";
          }
        );
        test = craneLib.cargoTest (
          commonArgs
          // {
            inherit cargoArtifacts;
            cargoTestExtraArgs = "--workspace --all-targets";
          }
        );
        pre-commit = pre-commit;

        # The shipped example configuration, run through the binary that has to
        # accept it. `--strict` turns every warning into a failure, so a key
        # renamed in the schema without being renamed in `config.toml` — or one
        # that quietly landed in the dropped-features table — fails CI instead
        # of printing a warning on the user's first start. A unit test already
        # checks that the example parses *equal to the defaults*; this checks
        # the same file through the real command-line path, which is the one
        # people copy in the README.
        example-config = pkgs.runCommand "topbar-example-config" { } ''
          ${lib.getExe topbar} --check-config --strict --config ${./config.toml} | tee $out
        '';
      };

      devShells.${system}.default = craneLib.devShell {
        checks = self.checks.${system};
        # The same bindgen wiring the build needs, so `cargo build` inside the
        # shell compiles the PipeWire bindings too.
        inputsFrom = [ topbar ];
        packages = with pkgs; [
          rust-analyzer
          niri
          grim
          headsetcontrol
          brightnessctl
          # `notify-send`, for driving the notification daemon by hand. Always
          # inside `dbus-run-session`: the panel under test must never be
          # pointed at the session bus the developer is logged into.
          libnotify
          # The visual smoke run's own tooling: `magick` tells a screenshot
          # that actually caught a popover from one the nested compositor had
          # not presented yet, and python3 serves the recorded API fixtures the
          # weather run is pointed at. Both were being borrowed from whatever
          # happened to be on the developer's PATH.
          imagemagick
          python3
          # `pulseaudio` and `pactl`, for the OSD smoke run. It starts a
          # sound server of its *own* inside the sandbox, with a null sink and
          # a PULSE_RUNTIME_PATH under the run's XDG box, and points both the
          # panel and the CLI at it. The developer's real PipeWire is on the
          # session they are logged into and must never hear from a test.
          pulseaudio
          # Synthetic input. niri offers zwlr_virtual_pointer_manager_v1 and
          # zwp_virtual_keyboard_manager_v1, so a nested session can be
          # *clicked* rather than only driven through the panel's own IPC —
          # which is how two dismissal bugs reached a real desktop with a
          # green test suite behind them. See scripts/smoke-pointer.sh.
          wlrctl
          wtype
        ];
        RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        shellHook = pre-commit.shellHook;
      };

      formatter.${system} = pkgs.nixfmt-tree;
    };
}
