{
  description = "Logos wallet backend module — coordinator + tx builder (multi-chain balances via Multicall3, send orchestration, local history).";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";

    # Dependency modules. Their published `.lidl` contracts drive the generated
    # `modules().<dep>` typed clients. The `follows` makes each dependency use the
    # SAME module-builder as this module (so `codegen.rust.source` is supported).
    # Local paths here for in-workspace development; replaced with follows/URLs
    # when registered in the workspace flake.
    eth_rpc_module = {
      url = "path:/Users/dlipicar/repos/logos-workspace/repos/eth-rpc-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    keystore_module = {
      url = "path:/Users/dlipicar/repos/logos-workspace/repos/keystore-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    token_list_module = {
      url = "path:/Users/dlipicar/repos/logos-workspace/repos/token-list-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
    in
    {
      packages = forAllSystems (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
