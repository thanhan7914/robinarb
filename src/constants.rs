//! Chain-fixed addresses and protocol constants for Robinhood Chain (chain id 4663).
//! Every address here is round-trip-verified on-chain, not taken from docs alone.

use alloy::primitives::{address, Address, U256};

pub const CHAIN_ID: u64 = 4663;

/// Canonical WETH on Robinhood Chain (bridged ERC20, not an OP-stack predeploy).
/// Confirmed via docs.robinhood.com/chain/contracts + DexScreener + token0() on
/// a live pair, all three agreeing.
pub const WETH: Address = address!("0Bd7D308f8E1639FAb988df18A8011f41EAcAD73");

/// USDG — the USD-stable anchor confirmed live day-one on this chain.
pub const USDG: Address = address!("5fc5360D0400a0Fd4f2af552ADD042D716F1d168");

/// Multicall3 — verified via eth_getCode at the canonical cross-chain
/// CREATE2 deployer address (safe to assume here, unlike the Uniswap v3
/// factory trap below).
pub const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

/// ArbGasInfo precompile (Arbitrum standard address, confirmed via
/// docs.robinhood.com/chain/protocol-contracts). Replaces Base's
/// GAS_PRICE_ORACLE (OP-stack predeploy, does not exist on Arbitrum chains).
pub const ARB_GAS_INFO: Address = address!("000000000000000000000000000000000000006C");

/// Address the deployless PoolQuoter is injected at via eth_call state
/// override (verify tooling only, never in the hot path). Any codeless
/// address works; this one is obviously synthetic.
pub const POOL_QUOTER_ADDR: Address = address!("000000000000000000000000000000000000bEEF");

/// Runtime bytecode of contracts/src/PoolQuoter.sol (solc 0.8.28, cancun,
/// optimizer 200) — no chain-specific logic. Quotes any UniV3-style pool BY
/// ADDRESS by executing its swap and reverting with the amounts in the
/// callback — unlike QuoterV2, which CREATE2-derives the pool from its own
/// factory and silently quotes the wrong pool for other forks.
/// Regenerate after editing the contract:
///   cd contracts && forge build && jq -r '.deployedBytecode.object' out/PoolQuoter.sol/PoolQuoter.json
pub const POOL_QUOTER_CODE: &str = "0x608060405234801561000f575f5ffd5b506004361061004a575f3560e01c806323a69e751461004e5780632c8958f61461004e57806342a5a00614610063578063fa461e331461004e575b5f5ffd5b61006161005c366004610272565b610088565b005b6100766100713660046102ee565b610098565b60405190815260200160405180910390f35b6100928484610243565b50505050565b5f836001600160a01b031663128acb08308585876100d4576100cf600173fffd8963efd1fc6a506488495d951d5263988d2661034f565b6100e4565b6100e46401000276a36001610374565b60405160e086901b6001600160e01b03191681526001600160a01b03948516600482015292151560248401526044830191909152909116606482015260a060848201525f60a482015260c40160408051808303815f875af1925050508015610169575060408051601f3d908101601f1916820190925261016691810190610393565b60015b6101f8573d808015610196576040519150601f19603f3d011682016040523d82523d5f602084013e61019b565b606091505b5080516040146101ad57805160208201fd5b5f5f828060200190518101906101c39190610393565b915091505f866101d357826101d5565b815b90505f81126101e4575f6101ed565b6101ed816103b5565b94505050505061023c565b505060405162461bcd60e51b81526020600482015260136024820152721cddd85c08191a59081b9bdd081c995d995c9d606a1b604482015260640160405180910390fd5b9392505050565b60408051602081018490529081018290525f906060016040516020818303038152906040529050805160208201fd5b5f5f5f5f60608587031215610285575f5ffd5b8435935060208501359250604085013567ffffffffffffffff8111156102a9575f5ffd5b8501601f810187136102b9575f5ffd5b803567ffffffffffffffff8111156102cf575f5ffd5b8760208284010111156102e0575f5ffd5b949793965060200194505050565b5f5f5f60608486031215610300575f5ffd5b83356001600160a01b0381168114610316575f5ffd5b92506020840135801515811461032a575f5ffd5b929592945050506040919091013590565b634e487b7160e01b5f52601160045260245ffd5b6001600160a01b03828116828216039081111561036e5761036e61033b565b92915050565b6001600160a01b03818116838216019081111561036e5761036e61033b565b5f5f604083850312156103a4575f5ffd5b505080516020909101519092909150565b5f600160ff1b82016103c9576103c961033b565b505f039056fea2646970667358221220bdd6b05101cc212c015854f72fb47dd21af4ca3d540fde3ef517b0f2a4563a6f64736f6c634300081c0033";

/// Runtime bytecode of contracts/src/FeeProbe.sol (solc 0.8.28, cancun,
/// optimizer 200). Unlike `POOL_QUOTER_CODE`, this is injected at TWO
/// caller-chosen REAL addresses at once (`holder`, already holding the token
/// under test, and `RELAY`, an ordinary never-exempted address) — see
/// `ingest/fee_probe.rs` for why one hop isn't enough. Regenerate after
/// editing the contract: cd contracts && forge build, then copy
/// out/FeeProbe.sol/FeeProbe.json .deployedBytecode.object
pub const FEE_PROBE_CODE: &str = "0x608060405234801561000f575f5ffd5b5060043610610034575f3560e01c80635a7b826814610038578063c4b1d38f1461005d575b5f5ffd5b61004b61004636600461034f565b610070565b60405190815260200160405180910390f35b61004b61006b366004610397565b6101d5565b60405163a9059cbb60e01b81526001600160a01b038481166004830152602482018390525f919086169063a9059cbb906044016020604051808303815f875af11580156100bf573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906100e391906103d1565b506040516370a0823160e01b81526001600160a01b0385811660048301525f91908716906370a0823190602401602060405180830381865afa15801561012b573d5f5f3e3d5ffd5b505050506040513d601f19601f8201168201806040525081019061014f91906103f7565b60405163c4b1d38f60e01b81526001600160a01b0388811660048301528681166024830152604482018390529192509086169063c4b1d38f906064016020604051808303815f875af11580156101a7573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906101cb91906103f7565b9695505050505050565b6040516370a0823160e01b81526001600160a01b0383811660048301525f9182918616906370a0823190602401602060405180830381865afa15801561021d573d5f5f3e3d5ffd5b505050506040513d601f19601f8201168201806040525081019061024191906103f7565b60405163a9059cbb60e01b81526001600160a01b038681166004830152602482018690529192509086169063a9059cbb906044016020604051808303815f875af1158015610291573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906102b591906103d1565b506040516370a0823160e01b81526001600160a01b0385811660048301528291908716906370a0823190602401602060405180830381865afa1580156102fd573d5f5f3e3d5ffd5b505050506040513d601f19601f8201168201806040525081019061032191906103f7565b61032b919061040e565b95945050505050565b80356001600160a01b038116811461034a575f5ffd5b919050565b5f5f5f5f60808587031215610362575f5ffd5b61036b85610334565b935061037960208601610334565b925061038760408601610334565b9396929550929360600135925050565b5f5f5f606084860312156103a9575f5ffd5b6103b284610334565b92506103c060208501610334565b929592945050506040919091013590565b5f602082840312156103e1575f5ffd5b815180151581146103f0575f5ffd5b9392505050565b5f60208284031215610407575f5ffd5b5051919050565b8181038181111561042d57634e487b7160e01b5f52601160045260245ffd5b9291505056fea2646970667358221220f1b664cd6e142e86402fd083b3f18b568badd7fff3b5ffa4735ef5af8dd6cbb564736f6c634300081c0033";

/// Uniswap V4 singleton PoolManager on Robinhood Chain — every V4 pool lives
/// inside it; all V4 events are emitted from this address, keyed by poolId
/// (topics[1]). Verified via eth_getCode (24,009 bytes present).
pub const UNIV4_POOL_MANAGER: Address = address!("8366a39cc670b4001a1121b8f6a443a643e40951");

/// Uniswap V4 StateView periphery (wraps PoolManager.extsload). Verified via
/// eth_getCode (3,531 bytes present).
pub const UNIV4_STATE_VIEW: Address = address!("f3334192d15450cdd385c8b70e03f9a6bd9e673b");

/// Official V4Quoter on Robinhood Chain. Verified via eth_getCode (6,118
/// bytes present). Verification only.
pub const UNIV4_QUOTER: Address = address!("8dc178efb8111bb0973dd9d722ebeff267c98f94");

/// UniswapV4 UniversalRouter. Verified via eth_getCode (24,546 bytes present).
pub const UNIV4_UNIVERSAL_ROUTER: Address = address!("8876789976decbfcbbbe364623c63652db8c0904");

/// Canonical cross-chain Permit2 (same address as everywhere else). Verified
/// via eth_getCode (9,152 bytes present).
pub const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");

/// PancakeSwap v3 factory on Robinhood Chain — round-trip verified via a
/// live pair's factory() call. Pancake v3 pools diverge from stock Uniswap
/// v3 storage layout, hence the separate `pancake_v3` kind kept in the
/// engine's DexTag.
pub const PANCAKE_V3_FACTORY: Address = address!("0BFBCF9fA4f9C56b0F40a671Ad40E0805A091865");

/// SushiSwap v3 factory — round-trip verified, stock Uniswap v3 bytecode
/// (maps to the generic `v3` kind, not a separate one).
pub const SUSHI_V3_FACTORY: Address = address!("E51960f1B45f1C9FB6D166E6a884F866fC70433B");

/// SwapHood v3 factory — chain-native DEX, round-trip verified, stock
/// Uniswap v3 bytecode (maps to the generic `v3` kind).
pub const SWAPHOOD_V3_FACTORY: Address = address!("0EC554f0BFf0Be6c99d1e95c8015bb0950f6A2C7");

/// Sheriff v2 factory — chain-native DEX, round-trip verified via
/// allPairsLength()/allPairs()/factory() (maps to the generic `v2` kind).
/// NOTE: Sheriff also has a DexScreener-labeled "v4" pool type at a distinct
/// 22.6KB contract that is NOT the Uniswap PoolManager — deliberately not
/// wired up here.
pub const SHERIFF_V2_FACTORY: Address = address!("10F7d1eF77F58181484936170430df13539C5162");

/// V4 PoolKey.fee sentinel: the pool's fee is set per-swap by its hook.
/// Vanilla-only policy drops these at discovery. Protocol-level constant,
/// unchanged from Base.
pub const UNIV4_DYNAMIC_FEE_FLAG: u32 = 0x80_0000;
/// Max valid static LP fee in pips (100%). Protocol-level constant, unchanged.
pub const UNIV4_MAX_LP_FEE: u32 = 1_000_000;

/// Uniswap V3 tick bounds. Protocol-level constant, unchanged from Base.
pub const MIN_TICK: i32 = -887272;
pub const MAX_TICK: i32 = 887272;

/// sqrt price bounds (exclusive limits for swaps). Protocol-level constant,
/// unchanged from Base.
pub fn min_sqrt_ratio() -> U256 {
    U256::from(4295128739u64)
}
pub fn max_sqrt_ratio() -> U256 {
    U256::from_str_radix("1461446703485210103287273052203988822378723970342", 10).unwrap()
}
