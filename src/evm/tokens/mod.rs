use std::{
    borrow::BorrowMut,
    cell::RefCell,
    collections::{hash_map, HashMap},
    fmt::Debug,
    ops::Deref,
    rc::Rc,
    str::FromStr,
    sync::Arc,
};

use alloy_primitives::hex;
use bytes::Bytes;
use libafl::schedulers::Scheduler;
use revm_interpreter::{CallContext, CallScheme, Contract, Interpreter};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::{
    types::{checksum, EVMFuzzState},
    vm::{EVMExecutor, EVMState, MEM_LIMIT},
};
use crate::{
    evm::{
        abi::{A256InnerType, AArray, AEmpty, BoxedABI, A256},
        onchain::endpoints::Chain,
        types::{EVMAddress, EVMU256},
    },
    generic_vm::{
        vm_executor::GenericVM,
        vm_state::{self, VMStateT},
    },
    input::ConciseSerde,
    is_call_success,
};

pub mod constant_pair;
pub mod uniswap;
pub mod v2_transformer;
pub mod weth_transformer;

// deposit
const SWAP_DEPOSIT: [u8; 4] = [0xd0, 0xe3, 0x0d, 0xb0];
// withdraw
const SWAP_WITHDRAW: [u8; 4] = [0x2e, 0x1a, 0x7d, 0x4d];
// swapExactETHForTokensSupportingFeeOnTransferTokens
const SWAP_BUY: [u8; 4] = [0xb6, 0xf9, 0xde, 0x95];
// swapExactTokensForETHSupportingFeeOnTransferTokens
const SWAP_SELL: [u8; 4] = [0x79, 0x1a, 0xc9, 0x47];

#[derive(Clone, Debug)]
pub enum UniswapProvider {
    PancakeSwap,
    SushiSwap,
    UniswapV2,
    UniswapV3,
    Biswap,
}

impl FromStr for UniswapProvider {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pancakeswap" => Ok(Self::PancakeSwap),
            "pancakeswapv2" => Ok(Self::PancakeSwap),
            "sushiswap" => Ok(Self::SushiSwap),
            "uniswapv2" => Ok(Self::UniswapV2),
            "uniswapv3" => Ok(Self::UniswapV3),
            "biswap" => Ok(Self::Biswap),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct UniswapInfo {
    pub pool_fee: usize,
    pub router: EVMAddress,
    pub factory: EVMAddress,
    pub init_code_hash: Vec<u8>,
}

pub trait PairContext {
    fn transform<VS, CI, SC>(
        &self,
        src: &EVMAddress,
        amount: EVMU256,
        state: &mut EVMFuzzState,
        vm: &mut EVMExecutor<VS, CI, SC>,
        reverse: bool,
    ) -> Option<(EVMAddress, EVMU256)>
    where
        VS: VMStateT + Default + 'static,
        CI: Serialize + DeserializeOwned + Debug + Clone + ConciseSerde + 'static,
        SC: Scheduler<State = EVMFuzzState> + Clone + 'static;

    fn name(&self) -> String;
}

#[derive(Clone)]
enum PairContextTy {
    Uniswap(Rc<RefCell<v2_transformer::UniswapPairContext>>),
    Weth(Rc<RefCell<weth_transformer::WethContext>>),
}

impl Debug for PairContextTy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairContextTy::Uniswap(ctx) => write!(f, "Uniswap({:?})", ctx.borrow()),
            PairContextTy::Weth(ctx) => write!(f, "Weth({:?})", ctx.borrow()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PathContext {
    pub route: Vec<PairContextTy>,
}

#[derive(Clone, Debug, Default)]
pub struct TokenContext {
    pub swaps: Vec<PathContext>,
    pub is_weth: bool,
    pub weth_address: EVMAddress,
}

static mut WETH_MAX: EVMU256 = EVMU256::ZERO;

impl TokenContext {
    pub fn buy<VS, CI, SC>(
        &self,
        amount_in: EVMU256,
        to: EVMAddress,
        state: &mut EVMFuzzState,
        vm_state: EVMState,
        vm: &mut EVMExecutor<VS, CI, SC>,
        seed: &[u8],
    ) -> Option<EVMState>
    where
        VS: VMStateT + Default + 'static,
        CI: Serialize + DeserializeOwned + Debug + Clone + ConciseSerde + 'static,
        SC: Scheduler<State = EVMFuzzState> + Clone + 'static,
    {
        vm.host.evmstate = vm_state;
        if self.is_weth {
            let ctx = self.swaps[0].route[0];
            if let PairContextTy::Weth(ctx) = ctx {
                ctx.deref()
                    .borrow_mut()
                    .transform(&self.weth_address, amount_in, state, vm, false);
            } else {
                panic!("Invalid weth context");
            }
        } else {
            if self.swaps.is_empty() {
                return None;
            }
            let mut current_amount_in = amount_in;
            let mut current_sender = None;
            let path_ctx = &self.swaps[seed[0] as usize % self.swaps.len()];
            for pair in path_ctx.route.iter().rev() {
                match pair {
                    PairContextTy::Uniswap(ctx) => {
                        if let Some((receiver, amount)) = ctx.deref().borrow_mut().transform(
                            &current_sender.unwrap(),
                            current_amount_in,
                            state,
                            vm,
                            true,
                        ) {
                            current_amount_in = amount;
                            current_sender = Some(receiver);
                        } else {
                            return None;
                        }
                    }
                    PairContextTy::Weth(ctx) => {
                        assert!(current_sender.is_none());
                        ctx.deref().borrow_mut().transform(&to, amount_in, state, vm, true);
                        current_sender = Some(to);
                    }
                }
            }
        }
        Some(vm.host.evmstate.clone())
    }

    // swapExactTokensForETHSupportingFeeOnTransferTokens
    pub fn sell<VS, CI, SC>(
        &self,
        amount_in: EVMU256,
        src: EVMAddress,
        state: &mut EVMFuzzState,
        vm_state: EVMState,
        vm: &mut EVMExecutor<VS, CI, SC>,
        seed: &[u8],
    ) -> Option<EVMState>
    where
        VS: VMStateT + Default + 'static,
        CI: Serialize + DeserializeOwned + Debug + Clone + ConciseSerde + 'static,
        SC: Scheduler<State = EVMFuzzState> + Clone + 'static,
    {
        if self.is_weth {
            let ctx = self.swaps[0].route[0];
            if let PairContextTy::Weth(ctx) = ctx {
                ctx.deref()
                    .borrow_mut()
                    .transform(&self.weth_address, amount_in, state, vm, false);
            } else {
                panic!("Invalid weth context");
            }
        } else {
            if self.swaps.is_empty() {
                return None;
            }
            let path_ctx = &self.swaps[seed[0] as usize % self.swaps.len()];
            let mut current_amount_in = amount_in;
            let mut current_sender = src;
            let path_ctx = &self.swaps[seed[0] as usize % self.swaps.len()];
            for pair in path_ctx.route.iter() {
                match pair {
                    PairContextTy::Uniswap(ctx) => {
                        if let Some((receiver, amount)) =
                            ctx.deref()
                                .borrow_mut()
                                .transform(&current_sender, current_amount_in, state, vm, false)
                        {
                            current_amount_in = amount;
                            current_sender = receiver;
                        } else {
                            return None;
                        }
                    }
                    PairContextTy::Weth(ctx) => {
                        ctx.deref()
                            .borrow_mut()
                            .transform(&current_sender, current_amount_in, state, vm, false);
                    }
                }
            }
        }
        Some(vm.host.evmstate.clone())
    }
}

pub fn get_uniswap_info(provider: &UniswapProvider, chain: &Chain) -> UniswapInfo {
    match (provider, chain) {
        (&UniswapProvider::UniswapV2, &Chain::BSC) => UniswapInfo {
            pool_fee: 25,
            router: EVMAddress::from_str("0x10ed43c718714eb63d5aa57b78b54704e256024e").unwrap(),
            factory: EVMAddress::from_str("0xca143ce32fe78f1f7019d7d551a6402fc5350c73").unwrap(),
            init_code_hash: hex::decode("00fb7f630766e6a796048ea87d01acd3068e8ff67d078148a3fa3f4a84f69bd5").unwrap(),
        },
        (&UniswapProvider::PancakeSwap, &Chain::BSC) => UniswapInfo {
            pool_fee: 25,
            router: EVMAddress::from_str("0x10ed43c718714eb63d5aa57b78b54704e256024e").unwrap(),
            factory: EVMAddress::from_str("0xca143ce32fe78f1f7019d7d551a6402fc5350c73").unwrap(),
            init_code_hash: hex::decode("00fb7f630766e6a796048ea87d01acd3068e8ff67d078148a3fa3f4a84f69bd5").unwrap(),
        },
        (&UniswapProvider::UniswapV2, &Chain::ETH) => UniswapInfo {
            pool_fee: 3,
            router: EVMAddress::from_str("0x7a250d5630b4cf539739df2c5dacb4c659f2488d").unwrap(),
            factory: EVMAddress::from_str("0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f").unwrap(),
            init_code_hash: hex::decode("96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f").unwrap(),
        },
        _ => panic!("Uniswap provider {:?} @ chain {:?} not supported", provider, chain),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SwapData {
    inner: HashMap<SwapType, SwapInfo>,
}

impl SwapData {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn push(&mut self, addr: &EVMAddress, abi: &mut BoxedABI) {
        if let Some(new) = SwapInfo::try_new(addr, abi) {
            // swap_infos with same type will be merged
            if let hash_map::Entry::Vacant(e) = self.inner.entry(new.ty) {
                e.insert(new);
            } else {
                self.inner.get_mut(&new.ty).unwrap().concat_path(new.path);
            }
        }
    }

    pub fn to_generic(&self) -> HashMap<String, vm_state::SwapInfo> {
        self.inner
            .iter()
            .map(|(k, v)| ((*k).into(), v.clone().into()))
            .collect()
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq, Hash)]
#[serde(into = "String")]
pub enum SwapType {
    #[default]
    Deposit,
    Buy,
    Withdraw,
    Sell,
}

impl From<SwapType> for String {
    fn from(ty: SwapType) -> Self {
        match ty {
            SwapType::Deposit => "deposit".to_string(),
            SwapType::Buy => "buy".to_string(),
            SwapType::Withdraw => "withdraw".to_string(),
            SwapType::Sell => "sell".to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SwapInfo {
    pub ty: SwapType,
    pub target: String,
    pub path: Vec<String>,
}

impl SwapInfo {
    pub fn try_new(target: &EVMAddress, abi: &mut BoxedABI) -> Option<Self> {
        let get_path = |abi: &mut BoxedABI, idx: usize| -> Option<Vec<String>> {
            if let Some(args) = abi.b.as_any().downcast_mut::<AArray>() {
                let path = args.data[idx]
                    .b
                    .as_any()
                    .downcast_ref::<AArray>()
                    .unwrap()
                    .data
                    .iter()
                    .map(|x| x.b.to_string())
                    .collect::<Vec<_>>();
                Some(path)
            } else {
                None
            }
        };

        let (ty, path) = match abi.function {
            SWAP_BUY => (SwapType::Buy, get_path(abi, 1)),
            SWAP_SELL => (SwapType::Sell, get_path(abi, 2)),
            SWAP_DEPOSIT => (SwapType::Deposit, Some(vec![])),
            SWAP_WITHDRAW => (SwapType::Withdraw, Some(vec![])),
            _ => return None,
        };

        if let Some(path) = path {
            let target = checksum(target);
            Some(Self { ty, target, path })
        } else {
            None
        }
    }

    pub fn concat_path(&mut self, new_path: Vec<String>) {
        // Find the first common element from the end
        let mut idx = self.path.len();
        for i in (0..self.path.len()).rev() {
            if self.path[i] == new_path[0] {
                idx = i;
                break;
            }
        }
        self.path.truncate(idx);
        self.path.extend(new_path);
    }
}

// Uniswap info -> Generic swap info
impl From<SwapInfo> for vm_state::SwapInfo {
    fn from(info: SwapInfo) -> Self {
        Self {
            ty: info.ty.into(),
            target: info.target,
            path: info.path,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use libafl::schedulers::StdScheduler;

    use super::TokenContext;
    use crate::{
        evm::{
            config::StorageFetchingMode,
            host::FuzzHost,
            input::ConciseEVMInput,
            onchain::{
                endpoints::{Chain, OnChainConfig},
                OnChain,
            },
            types::{generate_random_address, EVMFuzzState, EVMU256},
            vm::{EVMExecutor, EVMState},
        },
        state::FuzzState,
    };

    #[test]
    fn test_buy() {
        let state = FuzzState::new(0);
        let mut fuzz_host = FuzzHost::new(StdScheduler::new(), "work_dir".to_string());

        let onchain = OnChainConfig::new(Chain::BSC, 0);
        let onchain_mid = OnChain::new(onchain, StorageFetchingMode::OneByOne);
        fuzz_host.add_middlewares(Rc::new(RefCell::new(onchain_mid)));

        let mut evm_executor: EVMExecutor<EVMState, ConciseEVMInput, StdScheduler<EVMFuzzState>> =
            EVMExecutor::new(fuzz_host, generate_random_address(&mut state));

        let vm_state = EVMState::default();
        let ctx = TokenContext {
            swaps: vec![],
            is_weth: false,
            weth_address: generate_random_address(&mut state),
        };
        let r_addr = generate_random_address(&mut state);
        ctx.buy(
            EVMU256::from(200000),
            r_addr,
            &mut state,
            vm_state,
            &mut evm_executor,
            &[0],
        );
    }
}
//     use std::str::FromStr;

//     use tracing::debug;

//     use super::*;
//     use crate::evm::onchain::endpoints::Chain;

//     macro_rules! wrap {
//         ($x: expr) => {
//             Rc::new(RefCell::new($x))
//         };
//     }

//     #[test]
//     fn test_uniswap_sell() {
//         let t1 = TokenContext {
//             swaps: vec![PathContext {
//                 route: vec![wrap!(PairContext {
//                     pair_address:
// EVMAddress::from_str("0x0000000000000000000000000000000000000000").unwrap(),
//                     side: 0,
//                     uniswap_info:
// Arc::new(get_uniswap_info(&UniswapProvider::PancakeSwap, &Chain::BSC)),
//                     initial_reserves: (Default::default(),
// Default::default()),                     next_hop:
// EVMAddress::from_str("0x1100000000000000000000000000000000000000").unwrap(),
//                 })],
//                 final_pegged_ratio: EVMU256::from(1),
//                 final_pegged_pair: Rc::new(RefCell::new(None)),
//             }],
//             is_weth: false,
//             weth_address:
// EVMAddress::from_str("0xee00000000000000000000000000000000000000").unwrap(),
//             address:
// EVMAddress::from_str("0xff00000000000000000000000000000000000000").unwrap(),
//         };

//         let plan = generate_uniswap_router_sell(
//             &t1,
//             0,
//             EVMU256::from(10000),
//
// EVMAddress::from_str("0x2300000000000000000000000000000000000000").unwrap(),
//         );
//         debug!(
//             "plan: {:?}",
//             plan.unwrap()
//                 .iter()
//                 .map(|x| hex::encode(x.0.get_bytes()))
//                 .collect::<Vec<_>>()
//         );
//     }
// }
