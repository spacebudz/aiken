use pallas_primitives::{
    babbage::{CostMdls, MintedTx, Redeemer, TransactionInput, TransactionOutput},
    Fragment,
};
use pallas_traverse::{Era, MultiEraTx};

use error::Error;
pub use eval::get_script_and_datum_lookup_table;
pub use phase_one::eval_phase_one;
use script_context::{ResolvedInput, SlotConfig};

use crate::machine::cost_model::ExBudget;
use wasm_bindgen::prelude::*;

pub mod error;
mod eval;
mod phase_one;
pub mod script_context;
#[cfg(test)]
mod tests;
mod to_plutus_data;

/// Evaluate the scripts in a transaction using
/// the UPLC Cek Machine. This function collects
/// redeemers with ExUnits calculated from the evaluation.
/// You may optionally run a subset of phase one checks on
/// redeemers and scripts.
pub fn eval_phase_two(
    tx: &MintedTx,
    utxos: &[ResolvedInput],
    cost_mdls: Option<&CostMdls>,
    initial_budget: Option<&ExBudget>,
    slot_config: &SlotConfig,
    run_phase_one: bool,
) -> Result<Vec<Redeemer>, Error> {
    let redeemers = tx.transaction_witness_set.redeemer.as_ref();

    let lookup_table = get_script_and_datum_lookup_table(tx, utxos);

    if run_phase_one {
        // subset of phase 1 check on redeemers and scripts
        eval_phase_one(tx, utxos, &lookup_table)?;
    }

    match redeemers {
        Some(rs) => {
            let mut collected_redeemers = vec![];

            for redeemer in rs.iter() {
                let redeemer = eval::eval_redeemer(
                    tx,
                    utxos,
                    slot_config,
                    redeemer,
                    &lookup_table,
                    cost_mdls,
                    initial_budget,
                )?;

                collected_redeemers.push(redeemer)
            }

            Ok(collected_redeemers)
        }
        None => Ok(vec![]),
    }
}

#[wasm_bindgen]
pub struct RawUtxo {
    input: Vec<u8>,
    output: Vec<u8>,
}

#[wasm_bindgen]
impl RawUtxo {
    pub fn new(input: Vec<u8>, output: Vec<u8>) -> Self {
        Self { input, output }
    }
}

#[wasm_bindgen]
pub struct RawUtxos(Vec<RawUtxo>);

#[wasm_bindgen]
pub struct Budget {
    cpu: u64,
    mem: u64,
}

#[wasm_bindgen]
impl Budget {
    pub fn new(cpu: u64, mem: u64) -> Self {
        Self { cpu, mem }
    }
}

#[wasm_bindgen]
pub struct SlotConf {
    zero_time: u64,
    zero_slot: u64,
    slot_length: u64,
}

#[wasm_bindgen]
impl SlotConf {
    pub fn new(zero_time: u64, zero_slot: u64, slot_length: u64) -> Self {
        Self {
            zero_time,
            zero_slot,
            slot_length,
        }
    }
}

#[wasm_bindgen]
pub struct Red(Vec<u8>);

#[wasm_bindgen]
pub struct Redeemers(Vec<Red>);

/// This function is the same as [`eval_phase_two`]
/// but the inputs are raw bytes.
/// initial_budget expects (cpu, mem).
/// slot_config (zero_time, zero_slot, slot_length)
#[wasm_bindgen]
pub fn eval_phase_two_raw(
    tx_bytes: &[u8],
    utxos_bytes: &RawUtxos,
    cost_mdls_bytes: &[u8],
    initial_budget: &Budget,
    slot_config: SlotConf,
    run_phase_one: bool,
) -> Result<Redeemers, JsError> {
    let multi_era_tx = MultiEraTx::decode(Era::Babbage, tx_bytes)
        .or_else(|_| MultiEraTx::decode(Era::Alonzo, tx_bytes))
        .unwrap();

    let cost_mdls = CostMdls::decode_fragment(cost_mdls_bytes).unwrap();

    let budget = ExBudget {
        cpu: initial_budget.cpu as i64,
        mem: initial_budget.mem as i64,
    };

    let mut utxos = Vec::new();

    for utxo in utxos_bytes.0.iter() {
        utxos.push(ResolvedInput {
            input: TransactionInput::decode_fragment(&utxo.input).unwrap(),
            output: TransactionOutput::decode_fragment(&utxo.output).unwrap(),
        });
    }

    let sc = SlotConfig {
        zero_time: slot_config.zero_time,
        zero_slot: slot_config.zero_slot,
        slot_length: slot_config.slot_length,
    };

    match multi_era_tx {
        MultiEraTx::Babbage(tx) => {
            match eval_phase_two(
                &tx,
                &utxos,
                Some(&cost_mdls),
                Some(&budget),
                &sc,
                run_phase_one,
            ) {
                Ok(redeemers) => Ok(Redeemers(
                    redeemers
                        .iter()
                        .map(|r| Red(r.encode_fragment().unwrap()))
                        .collect(),
                )),
                Err(err) => Err(JsError::new(&err.to_string())),
            }
        }
        // MultiEraTx::AlonzoCompatible(tx, _) => match eval_tx(&tx, &utxos, &sc) {
        //     Ok(redeemers) => Ok(redeemers
        //         .iter()
        //         .map(|r| r.encode_fragment().unwrap())
        //         .collect()),
        //     Err(_) => Err(()),
        // },
        // TODO: I probably did a mistake here with using MintedTx which is only compatible with Babbage tx.
        _ => todo!("Wrong era. Please use babbage"),
    }
}
