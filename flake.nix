{
  description = "Logos wallet backend module — coordinator + tx builder (multi-chain balances via Multicall3, send orchestration, local history).";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";

    # Dependency modules. Their published `.lidl` contracts drive the generated
    # `modules().<dep>` typed clients. The `follows` makes each dependency use the
    # SAME module-builder as this module (so `codegen.rust.source` is supported).
    # In the workspace these resolve via `follows`/`--override-input` to the local
    # checkouts; standalone they build once the dependency repos' default branches
    # carry this code.
    eth_rpc_module = {
      url = "github:logos-co/logos-evm-eth-rpc-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    keystore_module = {
      url = "github:logos-co/logos-evm-keystore-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    token_list_module = {
      url = "github:logos-co/logos-evm-token-list-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    uniswap_module = {
      url = "github:logos-co/logos-evm-uniswap-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
      inputs.eth_rpc_module.follows = "eth_rpc_module";
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
