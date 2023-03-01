// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkVM library.

// The snarkVM library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkVM library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkVM library. If not, see <https://www.gnu.org/licenses/>.

use std::{collections::BTreeMap, sync::Arc};

use crate::{
    fft::{
        domain::{FFTPrecomputation, IFFTPrecomputation},
        DensePolynomial,
        EvaluationDomain,
        Evaluations as EvaluationsOnDomain,
    },
    snark::marlin::{
        ahp::indexer::Circuit,
        AHPError,
        MarlinMode,
    },
};
use snarkvm_fields::PrimeField;
use snarkvm_r1cs::{SynthesisError, SynthesisResult};

pub struct CircuitSpecificState<F: PrimeField> {
    pub(super) input_domain: EvaluationDomain<F>,
    pub(super) constraint_domain: EvaluationDomain<F>,
    pub(super) non_zero_a_domain: EvaluationDomain<F>,
    pub(super) non_zero_b_domain: EvaluationDomain<F>,
    pub(super) non_zero_c_domain: EvaluationDomain<F>,

    /// The number of instances being proved in this batch.
    pub(in crate::snark) batch_size: usize,

    /// The list of public inputs for each instance in the batch.
    /// The length of this list must be equal to the batch size.
    pub(super) padded_public_variables: Vec<Vec<F>>,

    /// The list of private variables for each instance in the batch.
    /// The length of this list must be equal to the batch size.
    pub(super) private_variables: Vec<Vec<F>>,

    /// The list of Az vectors for each instance in the batch.
    /// The length of this list must be equal to the batch size.
    pub(super) z_a: Option<Vec<Vec<F>>>,

    /// The list of Bz vectors for each instance in the batch.
    /// The length of this list must be equal to the batch size.
    pub(super) z_b: Option<Vec<Vec<F>>>,

    /// A list of polynomials corresponding to the interpolation of the public input.
    /// The length of this list must be equal to the batch size.
    pub(super) x_polys: Vec<DensePolynomial<F>>,

    /// Randomizers for z_b.
    /// The length of this list must be equal to the batch size.
    pub(super) mz_poly_randomizer: Option<Vec<F>>,

    /// Polynomials involved in the holographic sumcheck.
    pub(super) lhs_polynomials: Option<[DensePolynomial<F>; 3]>,
    /// Polynomials involved in the holographic sumcheck.
    pub(super) sums: Option<[F; 3]>,
}

/// State for the AHP prover.
pub struct State<'a, F: PrimeField> {
    pub(super) max_constraint_domain: EvaluationDomain<F>,
    pub(super) max_non_zero_domain: EvaluationDomain<F>,
    pub(super) circuit_specific_states: BTreeMap<&'a [u8; 32], CircuitSpecificState<F>>,
    pub(super) total_instances: usize,
    /// The first round oracles sent by the prover.
    /// The length of this list must be equal to the batch size.
    pub(in crate::snark) first_round_oracles: Option<Arc<super::FirstOracles<'a, F>>>,
    // / The challenges sent by the verifier in the first round
    // TODO: not sure yet if we actually need the following:
    // pub(super) verifier_first_message: Option<verifier::FirstMessage<'a, F, MM>>,
}

pub type PaddedPubInputs<F> = Vec<F>;
pub type PrivateInputs<F> = Vec<F>;
pub type Za<F> = Vec<F>;
pub type Zb<F> = Vec<F>;
pub struct Assignments<F>(
    pub PaddedPubInputs<F>,
    pub PrivateInputs<F>,
    pub Za<F>,
    pub Zb<F>
);

impl<'a, F: PrimeField, MM: MarlinMode> State<'a, F> {
    pub fn initialize(
        // TODO: which map should we use?
        // IndexMap or BTreeMap?
        indices_and_assignments: BTreeMap<
            &'a Circuit<F, MM>,
            Vec<Assignments<F>>
        >
    ) -> Result<Self, AHPError> {
        let mut max_constraint_domain: Option<EvaluationDomain<F>> = None;
        let mut max_non_zero_domain: Option<EvaluationDomain<F>> = None;
        let mut total_instances = 0;
        let circuit_specific_states = indices_and_assignments
            .into_iter()
            .map(|(circuit, variable_assignments)| {
                let index_info = &circuit.index_info;
                let constraint_domain = EvaluationDomain::new(index_info.num_constraints)
                    .ok_or(SynthesisError::PolynomialDegreeTooLarge)?;
                max_constraint_domain = match max_constraint_domain {
                    Some(max_d) => {
                        if max_d.size() < constraint_domain.size() {
                            Some(constraint_domain)
                        } else {
                            Some(max_d)
                        }
                    },
                    None => Some(constraint_domain),
                };

                let non_zero_a_domain =
                    EvaluationDomain::new(index_info.num_non_zero_a).ok_or(SynthesisError::PolynomialDegreeTooLarge)?;
                let non_zero_b_domain =
                    EvaluationDomain::new(index_info.num_non_zero_b).ok_or(SynthesisError::PolynomialDegreeTooLarge)?;
                let non_zero_c_domain =
                    EvaluationDomain::new(index_info.num_non_zero_c).ok_or(SynthesisError::PolynomialDegreeTooLarge)?;

                max_non_zero_domain = match max_non_zero_domain {
                    Some(max_d) => {
                        let max_domain_candidate = Self::max_non_zero_domain_helper(non_zero_a_domain, non_zero_b_domain, non_zero_c_domain);
                        if max_d.size() < max_domain_candidate {
                            Some(max_domain_candidate)
                        } else {
                            Some(max_d)
                        }
                    },
                    None => Some(max_non_zero_domain),
                };

                let mut input_domain = None; // TODO: we're in a single circuit, can we just efficiently/cleanly assign the first valid domain?
                let batch_size = variable_assignments.len();
                total_instances += batch_size;
                let mut z_as = Vec::with_capacity(batch_size);
                let mut z_bs = Vec::with_capacity(batch_size);
                let mut x_polys = Vec::with_capacity(batch_size);
                let mut padded_public_variables = Vec::with_capacity(batch_size);
                let mut private_variables = Vec::with_capacity(batch_size);

                for Assignments(padded_public_input, private_input, z_a, z_b) in variable_assignments {
                    z_as.push(z_a);
                    z_bs.push(z_b);
                    input_domain = input_domain.or_else(|| EvaluationDomain::new(padded_public_input.len()));
                    let x_poly = EvaluationsOnDomain::from_vec_and_domain(padded_public_input.clone(), input_domain.unwrap())
                            .interpolate();
                    x_polys.push(x_poly);
                    padded_public_variables.push(padded_public_input);
                    private_variables.push(private_input);
                }
                let input_domain = input_domain.unwrap();
                
                let state = CircuitSpecificState {
                    input_domain,
                    constraint_domain,
                    non_zero_a_domain,
                    non_zero_b_domain,
                    non_zero_c_domain,
                    batch_size,
                    padded_public_variables,
                    x_polys,
                    private_variables,
                    z_a: Some(z_as),
                    z_b: Some(z_bs),
                    mz_poly_randomizer: None,
                    lhs_polynomials: None,
                    sums: None,
                };
                Ok((circuit.hash, state))
            })
            .collect::<SynthesisResult<BTreeMap<_, _>>>()?;

        let max_constraint_domain = max_constraint_domain.ok_or(AHPError::BatchSizeIsZero)?;
        let max_non_zero_domain = max_non_zero_domain.ok_or(AHPError::BatchSizeIsZero)?;

        Ok(Self {
            max_constraint_domain,
            max_non_zero_domain,
            circuit_specific_states,
            total_instances,
            first_round_oracles: None,
        })
    }

    /// Get the batch size for a given circuit.
    pub fn batch_size(&self, circuit: &[u8; 32]) -> Option<usize> {
        self.circuit_specific_states.get(circuit).map(|s| s.batch_size)
    }

    /// Get the total batch size.
    pub fn total_batch_size(&self) -> usize {
        self.circuit_specific_states.values().map(|s| s.batch_size).sum()
    }

    /// Get the public inputs for the entire batch.
    pub fn public_inputs(&self, circuit: &[u8; 32]) -> Option<Vec<Vec<F>>> {
        self.circuit_specific_states.get(circuit).map(|s| s.padded_public_variables.iter().map(|v| super::ConstraintSystem::unformat_public_input(v)).collect())
    }

    /// Get the padded public inputs for the entire batch.
    pub fn padded_public_inputs(&self, circuit: &[u8; 32]) -> Option<Vec<Vec<F>>> {
        self.circuit_specific_states.get(circuit).map(|s| s.padded_public_variables)
    }

    // TODO: think about removing these getters, as the circuit already contains the information it is just double checking
    pub fn fft_precomputation(&self, circuit: &[u8; 32]) -> Option<&FFTPrecomputation<F>> {
        self.circuit_specific_states.contains_key(circuit).then(|| &circuit.fft_precomputation)
    }

    pub fn ifft_precomputation(&self, circuit: &[u8; 32]) -> Option<&IFFTPrecomputation<F>> {
        self.circuit_specific_states.contains_key(circuit).then(|| &circuit.ifft_precomputation)
    }
}
