use std::{collections::HashMap, fmt::Debug, marker::PhantomData, str::FromStr};

/// Corpus schedulers for ItyFuzz
/// Used to determine which input / VMState to fuzz next
use libafl::corpus::Corpus;
use libafl::{
    corpus::Testcase,
    prelude::{CorpusId, HasMetadata, HasTestcase, UsesInput},
    schedulers::{RemovableScheduler, Scheduler},
    state::{HasCorpus, UsesState},
    Error,
};
use libafl_bolts::impl_serdeany;
use revm_primitives::HashSet;
use serde::{Deserialize, Serialize};

use super::{
    host::{BRANCH_STATUS, BRANCH_STATUS_IDX},
    onchain::ADDR_CODE_ADDR,
    types::EVMAddress,
};
use crate::{
    evm::{
        abi::FUNCTION_SIG,
        blaz::builder::{ArtifactInfoMetadata, BuildJobResult},
        corpus_initializer::EVMInitializationArtifacts,
        input::EVMInput,
    },
    input::VMInputT,
    power_sched::{PowerMutationalStageWithId, TestcaseScoreWithId},
};

/// The status of the branch, whether it is covered on true, false or both
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BranchCoveredStatus {
    /// The branch is covered on true
    True,
    /// The branch is covered on false
    False,
    /// The branch is covered on both true and false
    Both,
}

impl BranchCoveredStatus {
    fn merge(&self, branch_status: bool) -> (Self, bool) {
        match self {
            Self::Both => (Self::Both, false),
            Self::True => {
                if branch_status {
                    (Self::True, false)
                } else {
                    (Self::Both, true)
                }
            }
            Self::False => {
                if branch_status {
                    (Self::Both, true)
                } else {
                    (Self::False, false)
                }
            }
        }
    }

    fn from(branch_status: bool) -> Self {
        if branch_status {
            Self::True
        } else {
            Self::False
        }
    }
}

/// The Metadata for uncovered branches
#[derive(Serialize, Deserialize, Clone, Debug)]
#[cfg_attr(
    any(not(feature = "serdeany_autoreg"), miri),
    allow(clippy::unsafe_derive_deserialize)
)] // for SerdeAny
pub struct UncoveredBranchesMetadata {
    branch_to_testcases: HashMap<(EVMAddress, usize), HashSet<CorpusId>>,
    testcase_to_uncovered_branches: HashMap<CorpusId, usize>,
    branch_status: HashMap<(EVMAddress, usize), BranchCoveredStatus>,
}

impl Default for UncoveredBranchesMetadata {
    fn default() -> Self {
        Self::new()
    }
}

impl UncoveredBranchesMetadata {
    /// Create new [`struct@UncoveredBranchesMetadata`]
    #[must_use]
    pub fn new() -> Self {
        Self {
            branch_to_testcases: HashMap::new(),
            testcase_to_uncovered_branches: HashMap::new(),
            branch_status: HashMap::new(),
        }
    }
}

impl_serdeany!(UncoveredBranchesMetadata);

/// The Metadata for each testcase used in ABI power schedules.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[cfg_attr(
    any(not(feature = "serdeany_autoreg"), miri),
    allow(clippy::unsafe_derive_deserialize)
)] // for SerdeAny
pub struct PowerABITestcaseMetadata {
    /// Number of lines in source code, initialized in on_add
    lines: usize,
}

impl PowerABITestcaseMetadata {
    /// Create new [`struct@SchedulerTestcaseMetadata`]
    #[must_use]
    pub fn new(lines: usize) -> Self {
        Self { lines }
    }
}

pub fn parse_sig_to_score(filename: &str) -> HashMap<(EVMAddress, String), usize> {
    let mut sig_to_score = HashMap::new();
    let content = std::fs::read_to_string(filename).unwrap();
    for line in content.lines() {
        let mut iter = line.split(',');
        let address = EVMAddress::from_str(iter.next().unwrap()).expect("address should be valid");
        let slug = iter.next().unwrap();
        let score = iter.next().unwrap().parse::<usize>().expect("score should be valid");
        sig_to_score.insert((address, slug.to_owned()), score);
    }
    sig_to_score
}

impl_serdeany!(PowerABITestcaseMetadata);

#[derive(Debug, Clone)]
pub struct PowerABIScheduler<S> {
    pub sig_to_score: HashMap<(EVMAddress, String), usize>,
    phantom: PhantomData<S>,
}

impl<S> Default for PowerABIScheduler<S> {
    fn default() -> Self {
        Self::new(HashMap::new())
    }
}

impl<S> PowerABIScheduler<S> {
    pub fn new(sig_to_score: HashMap<(EVMAddress, String), usize>) -> Self {
        Self {
            phantom: PhantomData,
            sig_to_score,
        }
    }

    fn add_abi_metadata(&mut self, testcase: &mut Testcase<EVMInput>) -> Result<(), Error> {
        let input = testcase.input().clone().unwrap();
        let address = input.get_contract();
        let tc_func = match input.get_data_abi() {
            Some(abi) => abi.function,
            None => {
                testcase.add_metadata(PowerABITestcaseMetadata::new(1));
                return Ok(()); // Some EVMInput don't have abi, like borrow
            }
        };
        let tc_func_name = unsafe {
            FUNCTION_SIG.get(&tc_func).unwrap_or_else(|| {
                panic!(
                    "function signature {} @ {:?} not found in FUNCTION_SIG",
                    hex::encode(tc_func),
                    input.get_contract()
                )
            })
        };
        let tc_func_slug = {
            let amount_args = tc_func_name.matches(',').count() + {
                if tc_func_name.contains("()") {
                    0
                } else {
                    1
                }
            };
            let name = tc_func_name.split('(').next().unwrap();
            format!("{}@{}", name, amount_args)
        };

        let real_addr = unsafe { ADDR_CODE_ADDR.as_ref().unwrap().get(&address).unwrap_or(&address) };

        testcase.add_metadata(PowerABITestcaseMetadata::new(
            self.sig_to_score
                .get(&(*real_addr, tc_func_slug.to_owned()))
                .unwrap_or(&1)
                .to_owned(),
        ));
        Ok(())
    }
}

impl<S> UsesState for PowerABIScheduler<S>
where
    S: UsesInput,
{
    type State = S;
}

impl<S> Scheduler for PowerABIScheduler<S>
where
    S: HasCorpus<Input = EVMInput> + HasTestcase + HasMetadata,
{
    fn on_add(&mut self, state: &mut Self::State, idx: CorpusId) -> Result<(), Error> {
        // adding power scheduling information based on code size
        {
            let mut testcase = state.testcase_mut(idx).unwrap();
            let input = testcase.input().clone().unwrap();
            {
                let current_idx = *state.corpus().current();
                testcase.set_parent_id_optional(current_idx);
            }
            if !input.is_step() {
                self.add_abi_metadata(&mut testcase)?;
            } else {
                testcase.add_metadata(PowerABITestcaseMetadata::new(1));
            }
        }

        // adding power scheduling information based on branch covered
        {
            let meta: &mut UncoveredBranchesMetadata =
                state.metadata_map_mut().get_mut::<UncoveredBranchesMetadata>().unwrap();
            let mut uncovered_counters = 0;

            let mut fullfilled = HashSet::new();

            for it in unsafe { BRANCH_STATUS.iter().take(BRANCH_STATUS_IDX) } {
                let (addr, pc, br) = it.unwrap();
                if fullfilled.contains(&(addr, pc)) {
                    continue;
                }

                match meta.branch_status.get_mut(&(addr, pc)) {
                    Some(v) => {
                        let (new_v, is_updated) = v.merge(br);

                        // remove all testcases that already cover this branch
                        if is_updated {
                            assert_eq!(new_v, BranchCoveredStatus::Both);
                            meta.branch_to_testcases
                                .get(&(addr, pc))
                                .expect("branch_to_testcases should contain this branch")
                                .iter()
                                .for_each(|tc_id| {
                                    if *tc_id == idx {
                                        return;
                                    }
                                    meta.testcase_to_uncovered_branches
                                        .entry(*tc_id)
                                        .and_modify(|e| *e -= 1)
                                        .or_insert(0);
                                });
                            meta.branch_to_testcases.remove(&(addr, pc));
                        } else {
                            // not fully covered, so add this testcase to the branch
                            meta.branch_to_testcases.entry((addr, pc)).or_default().insert(idx);
                            uncovered_counters += 1;
                        }

                        *v = new_v;
                    }
                    None => {
                        // not covered before, so no testcases cover this branch
                        meta.branch_status.insert((addr, pc), BranchCoveredStatus::from(br));

                        // not fully covered, so add this testcase to the branch
                        meta.branch_to_testcases.entry((addr, pc)).or_default().insert(idx);

                        uncovered_counters += 1;
                    }
                }

                fullfilled.insert((addr, pc));
            }

            // finally add the testcase to the uncovered_branches
            meta.testcase_to_uncovered_branches.insert(idx, uncovered_counters);
        }

        Ok(())
    }

    fn next(&mut self, state: &mut Self::State) -> Result<CorpusId, Error> {
        if state.corpus().count() == 0 {
            Err(Error::empty("No entries in corpus".to_owned()))
        } else {
            let id = state
                .corpus()
                .current()
                .map(|id| state.corpus().next(id))
                .flatten()
                .unwrap_or_else(|| state.corpus().first().unwrap());
            self.set_current_scheduled(state, Some(id))?;
            Ok(id)
        }
    }
}

impl<S> RemovableScheduler for PowerABIScheduler<S>
where
    S: HasCorpus<Input = EVMInput> + HasTestcase + HasMetadata,
{
    fn on_remove(
        &mut self,
        _state: &mut Self::State,
        _idx: CorpusId,
        _testcase: &Option<Testcase<<Self::State as UsesInput>::Input>>,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn on_replace(
        &mut self,
        _state: &mut Self::State,
        _idx: CorpusId,
        _prev: &Testcase<<Self::State as UsesInput>::Input>,
    ) -> Result<(), Error> {
        Ok(())
    }
}

pub trait ABIScheduler: Scheduler
where
    Self::State: HasCorpus,
{
    // on_add but with artifacts passed when state has no ArtifactInfoMetadata
    fn on_add_artifacts(
        &mut self,
        state: &mut Self::State,
        idx: CorpusId,
        artifacts: &EVMInitializationArtifacts,
    ) -> Result<(), Error>;
}

impl<S> ABIScheduler for PowerABIScheduler<S>
where
    S: HasCorpus<Input = EVMInput> + HasTestcase + HasMetadata,
{
    fn on_add_artifacts(
        &mut self,
        state: &mut S,
        idx: CorpusId,
        artifacts: &EVMInitializationArtifacts,
    ) -> Result<(), Error> {
        let mut testcase = state.testcase_mut(idx).unwrap();
        testcase.set_parent_id_optional(None);
        self.add_abi_metadata(&mut testcase)?;
        Ok(())
    }
}

/// The power assigned to each corpus entry
/// This result is used for power scheduling
#[derive(Debug, Clone)]
pub struct CorpusPowerABITestcaseScore<S> {
    phantom: PhantomData<S>,
}

impl<S> TestcaseScoreWithId<S> for CorpusPowerABITestcaseScore<S>
where
    S: HasCorpus + HasMetadata,
{
    fn compute(state: &S, entry: &mut Testcase<S::Input>, idx: CorpusId) -> Result<f64, Error> {
        let _num_lines = match entry.metadata::<PowerABITestcaseMetadata>() {
            Ok(meta) => meta.lines,
            Err(_e) => 1, // FIXME: should not happen
        };
        // TODO: more sophisticated power score
        let uncov_branch = {
            let meta = state.metadata_map().get::<UncoveredBranchesMetadata>().unwrap();
            meta.testcase_to_uncovered_branches.get(&idx).unwrap_or(&0).to_owned() + 1
        };

        let mut power = uncov_branch as f64 * 16.0 + _num_lines as f64 * 16.0;

        if power >= 3200.0 {
            power = 3200.0;
        }

        if power <= 32.0 {
            power = 32.0;
        }

        Ok(power)
    }
}

/// The standard powerscheduling stage
pub type PowerABIMutationalStage<E, EM, I, M, Z> =
    PowerMutationalStageWithId<E, CorpusPowerABITestcaseScore<<E as UsesState>::State>, EM, I, M, Z>;
