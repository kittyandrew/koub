{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {
    nixpkgs,
    crane,
    self,
    ...
  }: let
    system = "x86_64-linux";
    pkgs = nixpkgs.legacyPackages.${system};
    fenixPkgs = inputs.fenix.packages.${system};

    craneLib =
      (crane.mkLib pkgs).overrideToolchain
      fenixPkgs.minimal.toolchain;

    # Lint toolchain: minimal + rustfmt + clippy (for flake checks)
    lintCraneLib =
      (crane.mkLib pkgs).overrideToolchain
      fenixPkgs.default.toolchain;

    oubotRaw = craneLib.buildPackage {
      src = ./.;
      nativeBuildInputs = [pkgs.pkg-config];
      buildInputs = [pkgs.openssl pkgs.postgresql.lib];
    };

    oubot = pkgs.writeShellScriptBin "oubot" ''
      #!${pkgs.runtimeShell}

      # Run postgresql migrations.
      cp -r ${./migrations} ./migrations # Load migrations from source.
      ${pkgs.diesel-cli}/bin/diesel migration run

      # Finally, starting the actual program.
      ${oubotRaw}/bin/open-uptime-bot "$@"
    '';

    oubotCli = import ./cli/package.nix {inherit craneLib pkgs;};

    # ESP32-C3 cross-compilation toolchain with pre-built riscv32imc std.
    # @NOTE: Uses nightly + riscv32imc rust-std to avoid build-std (incompatible with crane).
    esp32CraneLib = (crane.mkLib pkgs).overrideToolchain (fenixPkgs.combine [
      fenixPkgs.latest.rustc
      fenixPkgs.latest.cargo
      fenixPkgs.targets."riscv32imc-unknown-none-elf".latest.rust-std
    ]);
    esp32Client = import ./clients/esp32/package.nix {craneLib = esp32CraneLib;};

    # Pico W (RP2040) cross-compilation toolchain with pre-built thumbv6m std.
    # @NOTE: Same pattern as ESP32 — nightly + target rust-std to avoid build-std.
    picoWCraneLib = (crane.mkLib pkgs).overrideToolchain (fenixPkgs.combine [
      fenixPkgs.latest.rustc
      fenixPkgs.latest.cargo
      fenixPkgs.targets."thumbv6m-none-eabi".latest.rust-std
    ]);
    picoWClient = import ./clients/pico-w/package.nix {craneLib = picoWCraneLib;};

    toNanosec = seconds: seconds * 1000000000; # Specify nanoseconds as per docker spec (LMAO).

    docker-image = pkgs.dockerTools.buildImage {
      name = "open-uptime-bot";
      tag = "2026.3.23";
      fromImage = pkgs.dockerTools.pullImage {
        imageName = "alpine";
        imageDigest = "sha256:25109184c71bdad752c8312a8623239686a9a2071e8825f20acb8f2198c3f659";
        sha256 = "sha256-gTKr5yQqJHECyXSyLA9GRT4Qm+ptahnRwy53W8Easb4=";
        finalImageTag = "3.23.3";
      };
      copyToRoot = pkgs.buildEnv {
        name = "image-root"; # @TODO: What is this name actually for?
        paths = [oubot oubotCli pkgs.curl]; # curl for healthcheck.
      };
      # @NOTE: Rocket.toml is very small and could be written here, but good for now.
      runAsRoot = ''
        #!${pkgs.runtimeShell}
        cp ${./Rocket.toml} /Rocket.toml
      '';
      config = {
        Cmd = ["${oubot}/bin/oubot"];
        Env = ["LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath [pkgs.openssl]}"];
        Healthcheck = {
          Test = ["CMD" "curl" "-sf" "0.0.0.0:8080/api/v1/health"];
          Interval = toNanosec 60;
          Timeout = toNanosec 3;
        };
      };
    };
  in {
    formatter.${system} = pkgs.alejandra;

    packages.${system} = {
      server = oubot;
      cli = oubotCli;
      esp32-client = esp32Client;
      pico-w-client = picoWClient;
      docker = docker-image;
    };

    devShells.${system}.default = with pkgs;
      mkShell {
        RUST_LOG = "info";
        nativeBuildInputs = [pkg-config git];
        buildInputs = [
          # Core Rust toolchain (complete — includes rustc, cargo, clippy, rust-src, rust-analyzer).
          fenixPkgs.complete.toolchain
          # Dev dependencies
          oubotCli
          openssl
          bore-cli
          diesel-cli
          shellcheck
          # Lint tools (also enforced by flake checks)
          python3Packages.black
          python3Packages.flake8
          python3Packages.isort
          alejandra
          deadnix
          # Runtime dependency
          postgresql.lib
          # ESP32 dev stuff.
          espflash
          # Pico W dev stuff.
          picotool
          elf2uf2-rs
          minicom
        ];
        shellHook = ''
          export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath [pkgs.openssl]}
          echo -e "\nWelcome to the shell :)\n"
        '';
      };

    checks.${system} = let
      checkArgs = test-script: {
        inherit pkgs;
        inherit system;
        inherit test-script;
        inherit oubot;
      };
      checkArgsWithCliBash = test-script: {
        inherit pkgs;
        inherit system;
        inherit test-script;
        inherit oubot;
        oubot-cli = oubotCli;
        test-script-type = "bash";
      };
      checkArgsWithDocker = test-script: {
        inherit pkgs;
        inherit system;
        inherit test-script;
        inherit oubot;
        inherit docker-image;
      };
      # @NOTE: security-auth test needs no test-script (inline Python in .nix),
      #  but lib.nix requires one. Pass a no-op script.
      noopScript = pkgs.writeText "noop" "true";
      # Pre-compiled deps for clippy (avoids rebuilding all dependencies each run)
      serverLintDeps = lintCraneLib.buildDepsOnly {
        src = ./.;
        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [pkgs.openssl pkgs.postgresql.lib];
      };
      cliLintDeps = lintCraneLib.buildDepsOnly {
        src = ./cli;
        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [pkgs.openssl];
      };
      # Dummy env vars for client build checks (pure mode can't read host env)
      dummyClientEnv = {
        OUBOT_WIFI_SSID = "ci-check";
        OUBOT_WIFI_PASS = "ci-check";
        OUBOT_SERVER = "http://ci:8080";
        OUBOT_TOKEN = "tk_cicheck12345678";
      };
    in {
      # Formatting: cargo fmt (all crates) + alejandra + black + isort
      fmt =
        pkgs.runCommand "fmt" {
          src = ./.;
          nativeBuildInputs = with pkgs; [
            fenixPkgs.default.toolchain
            python3Packages.black
            python3Packages.isort
            alejandra
          ];
        } ''
          export HOME=$(mktemp -d)
          cd $src && cargo fmt --check
          cd $src/cli && cargo fmt --check
          cd $src/clients/esp32 && cargo fmt --check
          cd $src/clients/pico-w && cargo fmt --check
          alejandra --check $src
          black --line-length=131 --check $src/tests/
          isort --check $src/tests/
          mkdir -p $out && touch $out/ok
        '';
      # Linting: clippy (server + CLI, crane for dep caching)
      clippy = lintCraneLib.cargoClippy {
        src = ./.;
        cargoArtifacts = serverLintDeps;
        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [pkgs.openssl pkgs.postgresql.lib];
        cargoClippyExtraArgs = "-- --deny warnings";
      };
      clippy-cli = lintCraneLib.cargoClippy {
        src = ./cli;
        cargoArtifacts = cliLintDeps;
        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [pkgs.openssl];
        cargoClippyExtraArgs = "-- --deny warnings";
      };
      # Linting: scripts + Nix + route-guard (shellcheck, flake8, deadnix)
      lint =
        pkgs.runCommand "lint" {
          src = ./.;
          nativeBuildInputs = with pkgs; [
            shellcheck
            python3Packages.flake8
            deadnix
            gnugrep
            gnused
            findutils
          ];
        } ''
          shellcheck $src/tests/*.sh
          flake8 --max-line-length=131 $src/tests/
          deadnix --fail --no-lambda-pattern-names $src

          # Route-guard lint: every handler must have a rate-limiting guard.
          # See src/main.rs @WARNING and src/bauth.rs RateLimitGuard.
          FAIL=0
          for file in $(find $src/src -name '*.rs'); do
            while IFS= read -r line_num; do
              sig=$(sed -n "$line_num,$((line_num+3))p" "$file")
              if ! echo "$sig" | grep -qE '(BAuth|AdminAuth|RateLimitGuard)'; then
                echo "FAIL: $(basename $file):$line_num - route handler missing rate-limit guard"
                echo "  $sig"
                FAIL=1
              fi
            done < <(grep -nE '#\[(get|post|put|patch|delete)\(' "$file" | cut -d: -f1)
          done
          if [ "$FAIL" = "1" ]; then
            echo "Every route handler must include BAuth, AdminAuth, or RateLimitGuard."
            exit 1
          fi

          mkdir -p $out && touch $out/ok
        '';
      # Client build checks (cross-compilation with dummy env vars)
      build-esp32 = let
        commonArgs =
          dummyClientEnv
          // {
            src = ./clients/esp32;
            doCheck = false;
            cargoExtraArgs = "--target riscv32imc-unknown-none-elf";
            postUnpack = ''
              sed -i '/^\[unstable\]$/d; /^build-std/d' $sourceRoot/.cargo/config.toml
            '';
          };
      in
        esp32CraneLib.buildPackage (commonArgs
          // {
            cargoArtifacts = esp32CraneLib.buildDepsOnly commonArgs;
          });
      build-pico-w = let
        commonArgs =
          dummyClientEnv
          // {
            src = ./clients/pico-w;
            doCheck = false;
            cargoExtraArgs = "--target thumbv6m-none-eabi";
            postUnpack = ''
              sed -i '/^\[unstable\]$/d; /^build-std/d' $sourceRoot/.cargo/config.toml
            '';
            CARGO_TARGET_THUMBV6M_NONE_EABI_RUSTFLAGS = "-C link-arg=-L${./clients/pico-w}";
          };
      in
        picoWCraneLib.buildPackage (commonArgs
          // {
            cargoArtifacts = picoWCraneLib.buildDepsOnly (commonArgs
              // {
                buildPhaseCargoCommand = "cargo check --release --target thumbv6m-none-eabi";
              });
          });
      api-v1-up-test-success = import ./tests/api-v1-up-test-success.nix (checkArgs ./tests/api-v1-up-test-success.py);
      api-v1-up-duration-message = import ./tests/api-v1-up-duration-message.nix (checkArgs ./tests/api-v1-up-duration-message.py);
      cli-lifecycle = import ./tests/cli-lifecycle.nix (checkArgsWithCliBash ./tests/cli-lifecycle.sh);
      cli-settings = import ./tests/cli-settings.nix (checkArgsWithCliBash ./tests/cli-settings.sh);
      cli-admin = import ./tests/cli-admin.nix (checkArgsWithCliBash ./tests/cli-admin.sh);
      security-auth = import ./tests/security-auth.nix (checkArgs noopScript);
      docker-e2e = import ./tests/docker-e2e.nix (checkArgsWithDocker noopScript);
    };
  };
}
