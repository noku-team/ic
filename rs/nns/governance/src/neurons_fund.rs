//! Implementation of functions for Matched Funding.

// TODO[NNS1-2619]: remove this
#![allow(dead_code)]
#![allow(unused)]

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

use ic_base_types::PrincipalId;
use ic_nervous_system_common::E8;
use ic_nervous_system_governance::maturity_modulation::BASIS_POINTS_PER_UNITY;
use ic_nns_common::pb::v1::NeuronId;
use ic_sns_swap::pb::v1::{LinearScalingCoefficient, NeuronsFundParticipationConstraints};

use rust_decimal::{
    prelude::{FromPrimitive, ToPrimitive},
    Decimal, RoundingStrategy,
};
use rust_decimal_macros::dec;

use crate::pb::v1::{
    neurons_fund_snapshot::NeuronsFundNeuronPortion as NeuronsFundNeuronPortionPb,
    NeuronsFundParticipation as NeuronsFundParticipationPb,
    NeuronsFundSnapshot as NeuronsFundSnapshotPb,
    SwapParticipationLimits as SwapParticipationLimitsPb,
};
use crate::{
    governance,
    neuron_store::{NeuronStore, NeuronsFundNeuron},
    pb::v1::{
        create_service_nervous_system::SwapParameters, governance_error, GovernanceError,
        IdealMatchedParticipationFunction, Neuron,
    },
};

/// This is a theoretical limit which should be smaller than any realistic amount of maturity
/// that practically needs to be reserved from the Neurons' Fund for a given SNS swap.
pub const MAX_THEORETICAL_NEURONS_FUND_PARTICIPATION_AMOUNT_ICP_E8S: u64 = 333_000 * E8;

// The maximum number of intervals for scaling ideal Neurons' Fund participation down to effective
// participation. Theoretically, this number should be greater than double the number of neurons
// participating in the Neurons' Fund. Although the currently chosen value is quite high, it is
// still significantly smaller than `usize::MAX`, allowing to reject an misformed
// SnsInitPayload.coefficient_intervals structure with obviously too many elements.
const MAX_LINEAR_SCALING_COEFFICIENT_VEC_LEN: usize = 100_000;

/// The Neurons' Fund should not participate in any SNS swap with more than this portion of its
/// overall maturity.
pub const MAX_NEURONS_FUND_PARTICIPATION_BASIS_POINTS: u128 = 1_000; // 10%

pub fn take_max_initial_neurons_fund_participation_percentage(x: u64) -> u64 {
    ((x as u128)
        .saturating_mul(MAX_NEURONS_FUND_PARTICIPATION_BASIS_POINTS)
        .saturating_div(BASIS_POINTS_PER_UNITY)
        .min(u64::MAX as u128)) as u64
}

/// The implmentation of `Decimal::from_u64` cannot fail.
pub fn u64_to_dec(x: u64) -> Decimal {
    Decimal::from_u64(x).unwrap()
}

/// The canonical converter from (non-negative) `Decimal` to `u64`.
pub fn dec_to_u64(x: Decimal) -> Result<u64, String> {
    if x.is_sign_negative() {
        return Err(format!("Cannot convert negative value {:?} to u64.", x));
    }
    // The same could be achieved via `x.round()`, but we opt for verbosity.
    let x = x.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven);
    // We already checked that 0 <= x; the only reason `to_u64` can fail at this point is overflow.
    Decimal::to_u64(&x)
        .ok_or_else(|| format!("Overflow while trying to convert value {:?} to u64.", x))
}

#[derive(Debug)]
pub enum LinearScalingCoefficientValidationError {
    // All fields are mandatory.
    UnspecifiedField(String),
    EmptyInterval {
        from_direct_participation_icp_e8s: u64,
        to_direct_participation_icp_e8s: u64,
    },
    DenominatorIsZero,
    // The slope should be between 0.0 and 1.0.
    NumeratorGreaterThanDenominator {
        slope_numerator: u64,
        slope_denominator: u64,
    },
}

impl ToString for LinearScalingCoefficientValidationError {
    fn to_string(&self) -> String {
        let prefix = "LinearScalingCoefficientValidationError: ";
        match self {
            Self::UnspecifiedField(field_name) => {
                format!("{}Field `{}` must be specified.", prefix, field_name)
            }
            Self::EmptyInterval {
                from_direct_participation_icp_e8s,
                to_direct_participation_icp_e8s,
            } => {
                format!(
                    "{}from_direct_participation_icp_e8s ({}) must be strictly less that \
                    to_direct_participation_icp_e8s ({})).",
                    prefix, from_direct_participation_icp_e8s, to_direct_participation_icp_e8s,
                )
            }
            Self::DenominatorIsZero => {
                format!("{}slope_denominator must not equal zero.", prefix)
            }
            Self::NumeratorGreaterThanDenominator {
                slope_numerator,
                slope_denominator,
            } => {
                format!(
                    "{}slope_numerator ({}) must be less than or equal \
                    slope_denominator ({})",
                    prefix, slope_numerator, slope_denominator,
                )
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedLinearScalingCoefficient {
    pub from_direct_participation_icp_e8s: u64,
    pub to_direct_participation_icp_e8s: u64,
    pub slope_numerator: u64,
    pub slope_denominator: u64,
    pub intercept_icp_e8s: u64,
}

impl From<ValidatedLinearScalingCoefficient> for LinearScalingCoefficient {
    fn from(value: ValidatedLinearScalingCoefficient) -> Self {
        Self {
            from_direct_participation_icp_e8s: Some(value.from_direct_participation_icp_e8s),
            to_direct_participation_icp_e8s: Some(value.to_direct_participation_icp_e8s),
            slope_numerator: Some(value.slope_numerator),
            slope_denominator: Some(value.slope_denominator),
            intercept_icp_e8s: Some(value.intercept_icp_e8s),
        }
    }
}

impl TryFrom<LinearScalingCoefficient> for ValidatedLinearScalingCoefficient {
    type Error = LinearScalingCoefficientValidationError;

    fn try_from(value: LinearScalingCoefficient) -> Result<Self, Self::Error> {
        let from_direct_participation_icp_e8s =
            value.from_direct_participation_icp_e8s.ok_or_else(|| {
                LinearScalingCoefficientValidationError::UnspecifiedField(
                    "from_direct_participation_icp_e8s".to_string(),
                )
            })?;
        let to_direct_participation_icp_e8s =
            value.to_direct_participation_icp_e8s.ok_or_else(|| {
                LinearScalingCoefficientValidationError::UnspecifiedField(
                    "to_direct_participation_icp_e8s".to_string(),
                )
            })?;
        let slope_numerator = value.slope_numerator.ok_or_else(|| {
            LinearScalingCoefficientValidationError::UnspecifiedField("slope_numerator".to_string())
        })?;
        let slope_denominator = value.slope_denominator.ok_or_else(|| {
            LinearScalingCoefficientValidationError::UnspecifiedField(
                "slope_denominator".to_string(),
            )
        })?;
        // Currently we only check that `intercept_icp_e8s` is specified, so the actual field value
        // is unchecked.
        let intercept_icp_e8s = value.intercept_icp_e8s.ok_or_else(|| {
            LinearScalingCoefficientValidationError::UnspecifiedField(
                "intercept_icp_e8s".to_string(),
            )
        })?;
        if to_direct_participation_icp_e8s <= from_direct_participation_icp_e8s {
            return Err(LinearScalingCoefficientValidationError::EmptyInterval {
                from_direct_participation_icp_e8s,
                to_direct_participation_icp_e8s,
            });
        }
        if slope_denominator == 0 {
            return Err(LinearScalingCoefficientValidationError::DenominatorIsZero);
        }
        if slope_numerator > slope_denominator {
            return Err(
                LinearScalingCoefficientValidationError::NumeratorGreaterThanDenominator {
                    slope_numerator,
                    slope_denominator,
                },
            );
        }
        Ok(Self {
            from_direct_participation_icp_e8s,
            to_direct_participation_icp_e8s,
            slope_numerator,
            slope_denominator,
            intercept_icp_e8s,
        })
    }
}

enum MaxNeuronsFundParticipationValidationError {
    // This value must be specified.
    Unspecified,
    // Does not make sense if no SNS neurons can be created.
    BelowSingleParticipationLimit {
        max_neurons_fund_participation_icp_e8s: NonZeroU64,
        min_participant_icp_e8s: u64,
    },
    // The Neuron's Fund should never provide over 50% of the collected funds.
    AboveHalfOfSwapIcpMax {
        max_neurons_fund_participation_icp_e8s: u64,
        half_of_max_icp_e8s: u64,
    },
}

impl ToString for MaxNeuronsFundParticipationValidationError {
    fn to_string(&self) -> String {
        let prefix = "MaxNeuronsFundParticipationValidationError: ";
        match self {
            Self::Unspecified => {
                format!(
                    "{}max_neurons_fund_participation_icp_e8s must be specified.",
                    prefix
                )
            }
            Self::BelowSingleParticipationLimit {
                max_neurons_fund_participation_icp_e8s,
                min_participant_icp_e8s,
            } => {
                format!(
                    "{}max_neurons_fund_participation_icp_e8s ({} > 0) \
                    should be greater than or equal min_participant_icp_e8s ({}).",
                    prefix, max_neurons_fund_participation_icp_e8s, min_participant_icp_e8s,
                )
            }
            Self::AboveHalfOfSwapIcpMax {
                max_neurons_fund_participation_icp_e8s,
                half_of_max_icp_e8s,
            } => {
                format!(
                    "{}max_neurons_fund_participation_icp_e8s ({}) \
                    should be less than or equal half_of_max_icp_e8s ({}).",
                    prefix, max_neurons_fund_participation_icp_e8s, half_of_max_icp_e8s,
                )
            }
        }
    }
}

#[derive(Debug)]
pub enum LinearScalingCoefficientVecValidationError {
    LinearScalingCoefficientsOutOfRange(usize),
    LinearScalingCoefficientsUnordered(
        ValidatedLinearScalingCoefficient,
        ValidatedLinearScalingCoefficient,
    ),
    IrregularLinearScalingCoefficients(ValidatedLinearScalingCoefficient),
    LinearScalingCoefficientValidationError(LinearScalingCoefficientValidationError),
}

impl ToString for LinearScalingCoefficientVecValidationError {
    fn to_string(&self) -> String {
        let prefix = "LinearScalingCoefficientVecValidationError: ";
        match self {
            Self::LinearScalingCoefficientsOutOfRange(num_elements) => {
                format!(
                    "{}coefficient_intervals (len={}) must contain at least 1 and at most {} elements.",
                    prefix,
                    num_elements,
                    MAX_LINEAR_SCALING_COEFFICIENT_VEC_LEN,
                )
            }
            Self::LinearScalingCoefficientsUnordered(left, right) => {
                format!(
                    "{}The intervals {:?} and {:?} are ordered incorrectly.",
                    prefix, left, right
                )
            }
            Self::IrregularLinearScalingCoefficients(interval) => {
                format!(
                    "{}The first interval {:?} does not start from 0.",
                    prefix, interval,
                )
            }
            Self::LinearScalingCoefficientValidationError(error) => {
                format!("{}{}", prefix, error.to_string())
            }
        }
    }
}

impl From<LinearScalingCoefficientVecValidationError> for Result<(), String> {
    fn from(value: LinearScalingCoefficientVecValidationError) -> Self {
        Err(value.to_string())
    }
}

#[derive(Debug)]
pub enum NeuronsFundParticipationConstraintsValidationError {
    RelatedFieldUnspecified(String),
    LinearScalingCoefficientVecValidationError(LinearScalingCoefficientVecValidationError),
}

impl ToString for NeuronsFundParticipationConstraintsValidationError {
    fn to_string(&self) -> String {
        let prefix = "NeuronsFundParticipationConstraintsValidationError: ";
        match self {
            Self::RelatedFieldUnspecified(related_field_name) => {
                format!("{}{} must be specified.", prefix, related_field_name,)
            }
            Self::LinearScalingCoefficientVecValidationError(error) => {
                format!("{}{}", prefix, error.to_string())
            }
        }
    }
}

impl From<NeuronsFundParticipationConstraintsValidationError> for Result<(), String> {
    fn from(value: NeuronsFundParticipationConstraintsValidationError) -> Self {
        Err(value.to_string())
    }
}

pub struct ValidatedNeuronsFundParticipationConstraints {
    pub min_direct_participation_threshold_icp_e8s: u64,
    pub max_neurons_fund_participation_icp_e8s: u64,
    pub coefficient_intervals: Vec<ValidatedLinearScalingCoefficient>,
}

impl From<ValidatedNeuronsFundParticipationConstraints> for NeuronsFundParticipationConstraints {
    fn from(value: ValidatedNeuronsFundParticipationConstraints) -> Self {
        Self {
            min_direct_participation_threshold_icp_e8s: Some(
                value.min_direct_participation_threshold_icp_e8s,
            ),
            max_neurons_fund_participation_icp_e8s: Some(
                value.min_direct_participation_threshold_icp_e8s,
            ),
            coefficient_intervals: value
                .coefficient_intervals
                .into_iter()
                .map(LinearScalingCoefficient::from)
                .collect(),
        }
    }
}

impl TryFrom<NeuronsFundParticipationConstraints> for ValidatedNeuronsFundParticipationConstraints {
    type Error = NeuronsFundParticipationConstraintsValidationError;

    fn try_from(value: NeuronsFundParticipationConstraints) -> Result<Self, Self::Error> {
        // Validate min_direct_participation_threshold_icp_e8s
        let min_direct_participation_threshold_icp_e8s = value
            .min_direct_participation_threshold_icp_e8s
            .ok_or_else(|| {
                Self::Error::RelatedFieldUnspecified(
                    "min_direct_participation_threshold_icp_e8s".to_string(),
                )
            })?;

        // Validate max_neurons_fund_participation_icp_e8s
        let max_neurons_fund_participation_icp_e8s = value
            .max_neurons_fund_participation_icp_e8s
            .ok_or_else(|| {
            Self::Error::RelatedFieldUnspecified(
                "max_neurons_fund_participation_icp_e8s".to_string(),
            )
        })?;

        // Validate coefficient_intervals length.
        if !(1..MAX_LINEAR_SCALING_COEFFICIENT_VEC_LEN + 1)
            .contains(&value.coefficient_intervals.len())
        {
            return Err(Self::Error::LinearScalingCoefficientVecValidationError(
                LinearScalingCoefficientVecValidationError::LinearScalingCoefficientsOutOfRange(
                    value.coefficient_intervals.len(),
                ),
            ));
        }

        // Validate individual coefficient_intervals elements, consuming value.
        let coefficient_intervals: Vec<ValidatedLinearScalingCoefficient> = value
            .coefficient_intervals
            .into_iter()
            .map(ValidatedLinearScalingCoefficient::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| {
                Self::Error::LinearScalingCoefficientVecValidationError(
            LinearScalingCoefficientVecValidationError::LinearScalingCoefficientValidationError(err)
        )
            })?;

        // Validate that coefficient_intervals forms a partitioning.
        let intervals = &coefficient_intervals;
        intervals
            .iter()
            .zip(intervals.iter().skip(1))
            .find(|(prev, this)| {
                prev.to_direct_participation_icp_e8s != this.from_direct_participation_icp_e8s
            })
            .map_or(Ok(()), |(prev, this)| {
                Err(Self::Error::LinearScalingCoefficientVecValidationError(
                    LinearScalingCoefficientVecValidationError::LinearScalingCoefficientsUnordered(
                        prev.clone(),
                        this.clone(),
                    ),
                ))
            })?;

        // Validate that coefficient_intervals starts from 0.
        if let Some(first_interval) = intervals.first() {
            if first_interval.from_direct_participation_icp_e8s != 0 {
                return Err(Self::Error::LinearScalingCoefficientVecValidationError(
                    LinearScalingCoefficientVecValidationError::IrregularLinearScalingCoefficients(
                        first_interval.clone(),
                    ),
                ));
            }
        }

        Ok(Self {
            min_direct_participation_threshold_icp_e8s,
            max_neurons_fund_participation_icp_e8s,
            coefficient_intervals,
        })
    }
}

// TODO: Make this available only for tests.
#[derive(Clone, Copy, Debug)]
pub struct BinSearchIter {
    left: u128,
    x: u64,
    right: u128,
    y: Decimal,
}

/// An invertible function is a function that has an inverse (a.k.a. monotonically non-decreasing).
///
/// Say we have an invertible function `f(x: u64) -> u64` and its inverse is `g(y: u64) -> u64`.
/// Then the equality `g(f(x)) = x` must hold for all `x` s.t. `g(f(x))` is defined.
///
/// Additionally, the equality `f(g(y)) = y` must hold for all `y` s.t. `f(g(y))` is defined.
pub trait InvertibleFunction {
    /// A monotonically non-decreasing function.
    fn apply(&self, x: u64) -> Decimal;

    /// This method searches an inverse of `y` given the function defined by `apply`.
    ///
    /// An error is returned if the function defined by `apply` is not monotonically increasing.
    fn invert(&self, target_y: Decimal) -> Result<u64, String> {
        let (_, result) = self.invert_with_tracing(target_y);
        result
    }

    /// Like `invert`, but with extra output that can be used for testing and debugging.
    fn invert_with_tracing(&self, target_y: Decimal) -> (Vec<BinSearchIter>, Result<u64, String>) {
        // Used for testing and debugging
        let mut trace = vec![];
        if target_y.is_sign_negative() {
            return (
                trace,
                Err(format!("Cannot invert negative value {}.", target_y)),
            );
        }

        let mut left: u128 = 0;
        let mut right: u128 = u64::MAX.into();
        // Declaring `x` and `y` outside of the loop to be able to return the "best effort" result
        // in case the exact search fails (e.g., due to rounding errors).
        let mut x = ((left + right) / 2) as u64;
        let mut y = self.apply(x);

        // Stores the previously computed coordinates needed for monotonicity checks.
        let mut prev_coords: Option<(u64, Decimal)> = None;

        // This loop can run at least one and at most 64 iterations.
        while left <= right {
            // [Spec] assume loop guard: left <= right
            // [Spec] assume invariant (I): 0 <= left <= right+1, 0 <= right <= u64::MAX
            // [Spec] assume invariant (II): let((x0,_))=prev_coords && left < right+1 ==> x0 != ((left + right) / 2)

            x = ((left + right) / 2) as u64;
            // [Spec] assert(*) left <= x <= right

            y = self.apply(x);

            trace.push(BinSearchIter { left, x, right, y });

            // Error out if the function is not monotonic between x0 and x.
            if let Some((x0, y0)) = prev_coords {
                // The following assertion cannot fail due to invariant (II) in conjunction with
                // the loop guard.
                assert!(
                    x != x0,
                    "Invariant violated in InvertibleFunction.invert({})",
                    target_y
                );
                if (x > x0 && y < y0) || (x < x0 && y > y0) {
                    return (
                        trace,
                        Err(format!(
                        "Cannot invert value {} of a function that is not monotonically increasing \
                        between {:?} and {:?}.",
                        target_y,
                        std::cmp::min((x0, y0), (x, y)),
                        std::cmp::max((x0, y0), (x, y)),
                    )),
                    );
                }
            }
            prev_coords = Some((x, y));

            match y.cmp(&target_y) {
                Ordering::Equal => {
                    return (trace, Ok(x));
                }
                Ordering::Less => {
                    // y is too small <==> x is too small.
                    left = (x as u128) + 1;

                    // [Spec] assert invariant (I): 0 <= left <= right+1, 0 <= right <= u64::MAX
                    // [Spec] -- `left==x+1`; `right` did not change.
                    // [Spec] assert invariant (I): 0 <= x+1 <= right+1
                    // [Spec] -- given `0 <= x` from (*), we know that `0 <= x+1`.
                    // [Spec] -- `x+1 <= right+1`  <==>  `x <= right`.
                    // [Spec] -- `x <= right` follows from (*). QED (I)
                    // ---------------------------------------------------------------------------------
                    // [Spec] assert invariant (II): let((x0,_))=prev_coords && left < right+1 ==> x0 != ((left + right) / 2)
                    // [Spec] -- `prev_coords==(x, y)`; `left==x+1`; `right` did not change.
                    // [Spec] -- Assume left-hand side of `==>`: `let((x,_))=prev_coords && x < right`.
                    // [Spec] -- To prove: right-hand side of `==>`: `x != (x+1 + right) / 2`.
                    // [Spec] assert invariant (II): x != (x+1 + right) / 2
                    // [Spec] assert invariant (II): 2*x != (x+1 + right) + d
                    // [Spec] -- for some `d`: `0.0 <= d < 1.0`
                    // [Spec] assert invariant (II): x != right + (d + 1)
                    // [Spec] -- given `x < right` from left-hand side, we know that `x < right + 1 + d`. QED (II)
                }
                Ordering::Greater if x == 0 => {
                    // This currently cannot happen for a subtle reason (unless `target_y` is an
                    // invalid value). `x == 0` implies that either (1) `x==left==right==0`,
                    // or (2) `x==left==0` and `right==1`.
                    //
                    // Option (1) would mean that the measured value `y` is `f(x)`, which by
                    // assumption that the function cannot decrease, implies that `y` is the global
                    // minimum of `f`; thus, it cannot be that `y > target_y`, unless the caller
                    // is trying to invert a value that cannot be inverted.
                    //
                    // Option (2) would mean that the search has always been taking the `Ordering::Less`
                    // branch; otherwise, `left` would not still be at `0`. However, by moving `right`
                    // from its original value `u64::MAX` towards zero, one cannot reach `right==1`.
                    //
                    // This strategy can be described as "error-out if invalid inputs are detected;
                    // otherwise, round to the nearest". For example, for a function `f` s.t.
                    // `f(0) = 1.0000001` and `target_t = 1.0`, the result is an error (the input
                    // is deemed invalid as there does not exist an inverse in `1.0`). However, for
                    // a function `f` s.t. `f(100) = 0.0`, `f(101) = 1.0000001`, and `target_t = 1.0`,
                    // the result is `Ok(101)`, as we round to the nearest.
                    return (
                        trace,
                        Err(format!("Cannot invert small value {}.", target_y)),
                    );
                }
                Ordering::Greater => {
                    // `x == 0` is covered by the special case above.
                    // [Spec] assert x > 0

                    // y is too large <==> x is too large.

                    // [Spec] assert(**) 0 < x
                    right = (x as u128) - 1;

                    // [Spec] assert invariant (I): 0 <= left <= right+1, 0 <= right <= u64::MAX
                    // [Spec] -- `left` did not change; `right==x-1`.
                    // [Spec] assert: 0 <= left <= x-1+1, 0 <= x-1 <= u64::MAX
                    // [Spec] assert: 0 <= left <= x,     0 <= x-1 <= u64::MAX
                    // [Spec] -- `left <= x` follows from (*).
                    // [Spec] -- given `0 < x` from (**), we know that `0 <= x-1`. QED (I)
                    // ---------------------------------------------------------------------------------
                    // [Spec] assert invariant (II): let((x0,_))=prev_coords && left < right+1 ==> x0 != ((left + right) / 2)
                    // [Spec] -- `prev_coords==(x, y)`; `left` did not change; `right==x-1`.
                    // [Spec] -- Assume left-hand side if `==>`: `let((x,_))=prev_coords && left < x`.
                    // [Spec] -- To prove: right-hand side of `==>`: `x != (left + x-1) / 2`.
                    // [Spec] assert: x != (left + x-1) / 2
                    // [Spec] assert: 2*x != (left + x-1) + d
                    // [Spec] -- for some `d`: `0.0 <= d < 1.0`
                    // [Spec] assert: x + (1-d) != left
                    // [Spec] -- `0.0 < 1-d <= 1.0`.
                    // [Spec] given `left < x` from assumed left-hand side, we know that `x + (1-d) != left`. QED (II)
                }
            }
        }
        // If the search did not find the exact inverse value of `target_y`, we return the best of
        // the last two values.
        if let Some((x0, y0)) = prev_coords {
            if (target_y - y).abs() < (target_y - y0).abs() {
                (trace, Ok(x))
            } else {
                // Pretend that we knew the last iteration of the loop has been redundant.
                trace.pop();
                (trace, Ok(x0))
            }
        } else {
            unreachable!("Found a bug in InvertibleFunction.invert({:?})", target_y);
        }
    }
}

pub trait SerializableFunction {
    fn serialize(&self) -> String;
}

pub trait IdealMatchingFunction:
    InvertibleFunction + SerializableFunction + std::fmt::Debug
{
}

impl<F: InvertibleFunction + SerializableFunction + std::fmt::Debug> IdealMatchingFunction for F {}

#[derive(Debug)]
pub struct SimpleLinearFunction {}

impl SimpleLinearFunction {
    /// Attempts to create an instance of `Self` from a serialized representation, `repr`.
    pub fn new(repr: &String) -> Result<Self, String> {
        if repr == "<SimpleLinearFunction>" {
            Ok(Self {})
        } else {
            Err(format!(
                "Cannot deserialize `{}` as SimpleLinearFunction",
                repr
            ))
        }
    }
}

impl InvertibleFunction for SimpleLinearFunction {
    fn apply(&self, x: u64) -> Decimal {
        u64_to_dec(x)
    }
}

impl SerializableFunction for SimpleLinearFunction {
    fn serialize(&self) -> String {
        "<SimpleLinearFunction>".to_string()
    }
}

pub trait Interval {
    fn from(&self) -> u64;
    fn to(&self) -> u64;
    fn contains(&self, x: u64) -> bool {
        self.from() <= x && x < self.to()
    }
}

impl Interval for ValidatedLinearScalingCoefficient {
    fn from(&self) -> u64 {
        self.from_direct_participation_icp_e8s
    }

    fn to(&self) -> u64 {
        self.to_direct_participation_icp_e8s
    }
}

pub trait IntervalPartition<I> {
    fn intervals(&self) -> Vec<&I>;

    fn find_interval(&self, x: u64) -> Option<&I>
    where
        I: Interval,
    {
        let intervals = &self.intervals();
        if intervals.is_empty() {
            return None;
        }
        let mut i = 0_usize;
        // Cannot underflow as intervals.len() >= 1.
        let mut j = intervals.len() - 1;
        while i <= j {
            // [Spec] assume loop guard: i <= j
            // [Spec] assume invariant: 0 <= i <= j+1, 0 <= j < intervals.len()

            // Without `as u32`, an overflow would occur if e.g. `i==j==usize::MAX-1`
            // Converting back to usize is safe, as the average is npt greater than `j: usize`.
            let m = (((i as u32) + (j as u32)) / 2) as usize;
            // [Spec] assert(*) i <= m <= j  -- from math.
            if intervals[m].to() <= x {
                // If x == intervals[m].to, then x \in intervals[m+1]; move rightwards.
                // ... [intervals[m].from, intervals[m].to) ... x ...
                i = m + 1;
                // [Spec] assert invariant: 0 <= i   <= j+1, 0 <= j < intervals.len()
                // [Spec] -- `i==m+1`; `j` did not change.
                // [Spec] assert: 0 <= m+1 <= j+1
                // [Spec] -- given `0 <= m` from (*), we know that `0 <= m+1`.
                // [Spec] -- `m+1 <= j+1`  <==>  `m <= j`.
                // [Spec] -- `m <= j` follows from (*). QED
            } else if x < intervals[m].from() {
                // exclusive, since x==intervals[m].from ==> x \in intervals[m]; move leftwards.
                // ... x ... [intervals[m].from, intervals[m].to) ...
                if m == 0 {
                    // The leftmost interval starts from a value greated than `x`.
                    return None;
                }
                // [Spec] assert(**) 0 < m
                j = m - 1;
                // [Spec] assert invariant: 0 <= i <= j+1, 0 <= j < intervals.len()
                // [Spec] -- `i` did not change; `j==m-1`.
                // [Spec] assert: 0 <= i <= m-1+1, 0 <= m-1 < intervals.len()
                // [Spec] assert: 0 <= i <= m,     0 <= m-1 < intervals.len()
                // [Spec] -- `i <= m` follows from (*).
                // [Spec] -- given `0 < m` from (**), we know that `0 <= m-1`. QED
            } else {
                // x \in intervals[m]
                return Some(intervals[m]);
            }
        }
        None
    }
}

impl IntervalPartition<ValidatedLinearScalingCoefficient>
    for ValidatedNeuronsFundParticipationConstraints
{
    fn intervals(&self) -> Vec<&ValidatedLinearScalingCoefficient> {
        self.coefficient_intervals.iter().collect()
    }
}

pub struct NeuronsInterval<T> {
    from_direct_participation_icp_e8s: u64,
    to_direct_participation_icp_e8s: Option<u64>,
    neurons: Vec<T>,
}

impl<T> Interval for NeuronsInterval<T> {
    fn from(&self) -> u64 {
        self.from_direct_participation_icp_e8s
    }

    fn to(&self) -> u64 {
        self.to_direct_participation_icp_e8s.unwrap_or(u64::MAX)
    }
}

impl<T> IntervalPartition<NeuronsInterval<T>> for Vec<NeuronsInterval<T>> {
    fn intervals(&self) -> Vec<&NeuronsInterval<T>> {
        self.iter().collect()
    }
}

pub struct MatchedParticipationFunction {
    function: Box<dyn Fn(u64) -> Decimal>,
    params: ValidatedNeuronsFundParticipationConstraints,
}

impl MatchedParticipationFunction {
    pub fn new(
        function: Box<dyn Fn(u64) -> Decimal>,
        params: ValidatedNeuronsFundParticipationConstraints,
    ) -> Result<Self, String> {
        Ok(Self { function, params })
    }

    pub fn apply(&self, direct_participation_icp_e8s: u64) -> Decimal {
        // Normally, this threshold follows from `self.function`, a.k.a. the "ideal" participation
        // matching function. However, we add an explicit check here in order to make this
        // threashold more prominantly visible from readong the code. In addition, having this
        // branch allows us to use functions with a less complicated shape in the tests.
        if direct_participation_icp_e8s < self.params.min_direct_participation_threshold_icp_e8s {
            return dec!(0.0);
        }

        let intervals = &self.params.coefficient_intervals;
        // This condition is always satisfied, as `self.params` has been validated. We add it here
        // again for verbosity.
        assert!(
            !intervals.is_empty(),
            "There must be at least one interval."
        );

        // Special case A: direct_participation_icp_e8s is less than the first interval.
        if direct_participation_icp_e8s
            < intervals.first().unwrap().from_direct_participation_icp_e8s
        {
            // This should not happen in practice, as the first interval should contain 0.
            return dec!(0.0);
        }

        // Special case B: direct_participation_icp_e8s is greated than or equal to the last
        // interval's upper bound.
        if intervals.last().unwrap().to_direct_participation_icp_e8s <= direct_participation_icp_e8s
        {
            return u64_to_dec(u64::min(
                self.params.max_neurons_fund_participation_icp_e8s,
                MAX_THEORETICAL_NEURONS_FUND_PARTICIPATION_AMOUNT_ICP_E8S,
            ));
        }

        // Otherwise, direct_participation_icp_e8s must fall into one of the intervals.
        if let Some(ValidatedLinearScalingCoefficient {
            slope_numerator,
            slope_denominator,
            intercept_icp_e8s,
            ..
        }) = self.params.find_interval(direct_participation_icp_e8s)
        {
            // This value is how much of Neurons' Fund maturity we should "ideally" allocate.
            let ideal = (self.function)(direct_participation_icp_e8s);

            // Convert to Decimal
            let intercept_icp_e8s = u64_to_dec(*intercept_icp_e8s);
            let slope_numerator = Decimal::from(*slope_numerator);
            let slope_denominator = Decimal::from(*slope_denominator);

            // Normally, `self.params.max_neurons_fund_participation_icp_e8s` should be set to a
            // *reasonable* value. Since this value is computed based on the overall amount of
            // maturity in the Neurons' Fund (at the time when the swap is being opened), in theory
            // it could grow indefinitely. To safeguard against overly massive Neurons' Fund
            // participation to a single SNS swap, the NNS Governance (which manages the
            // Neurons' Fund) should limit the Neurons' Fund maximal theoretically possible amount
            // of participation also by `MAX_THEORETICAL_NEURONS_FUND_PARTICIPATION_AMOUNT_ICP_E8S`.
            // Here, we apply this threshold again for making it more explicit.
            let hard_cap = u64_to_dec(u64::min(
                self.params.max_neurons_fund_participation_icp_e8s,
                MAX_THEORETICAL_NEURONS_FUND_PARTICIPATION_AMOUNT_ICP_E8S,
            ));

            // This value is how much of Neurons' Fund maturity can "effectively" be allocated.
            // This value may be less than or equal to the "ideal" value above, due to:
            // (1) Some Neurons' fund neurons being too small to participate at all (at this direct
            //     participation amount, `direct_participation_icp_e8s`). This is taken into account
            //     via the `(slope_numerator / slope_denominator)` factor.
            // (2) Some Neurons' fund neurons being too big to fully participate (at this direct
            //     participation amount, `direct_participation_icp_e8s`). This is taken into account
            //     via the `intercept_icp_e8s` component.
            // (3) The computed overall participation amount (unexpectedly) exceeded `hard_cap`; so
            //     we enforce the limited at `hard_cap`.
            let effective = hard_cap.min(intercept_icp_e8s.saturating_add(
                // slope_denominator can't be zero as it has been validated.
                // See `LinearScalingCoefficientValidationError::DenominatorIsZero`.
                (slope_numerator / slope_denominator).saturating_mul(ideal),
            ));
            return effective;
        }

        unreachable!(
            "Found a bug in MatchedParticipationFunction.apply({})",
            direct_participation_icp_e8s
        );
    }
}

#[cfg(test)]
mod matched_participation_function_tests {
    use super::{
        dec_to_u64, u64_to_dec, InvertibleFunction, MatchedParticipationFunction,
        SimpleLinearFunction,
    };
    use crate::neurons_fund::ValidatedNeuronsFundParticipationConstraints;
    use ic_nervous_system_common::E8;
    use ic_sns_swap::pb::v1::{LinearScalingCoefficient, NeuronsFundParticipationConstraints};
    use rust_decimal::{
        prelude::{FromPrimitive, ToPrimitive},
        Decimal,
    };
    use rust_decimal_macros::dec;

    /// Used for testing; should be implemented as a closed form formula.
    trait AnalyticallyInvertibleFunction {
        fn invert_analytically(&self, target_y: Decimal) -> Result<u64, String>;
    }

    impl AnalyticallyInvertibleFunction for SimpleLinearFunction {
        fn invert_analytically(&self, target_y: Decimal) -> Result<u64, String> {
            dec_to_u64(target_y)
        }
    }

    #[test]
    fn test_simple_linear_function() {
        let f = SimpleLinearFunction {};
        let run_test_for_a = |x: u64| {
            let y = f.apply(x);
            let x1 = f.invert(y).unwrap();
            assert_eq!(x, x1);
        };
        let run_test_for_b = |y: Decimal| {
            let x = f.invert(y).unwrap();
            let y1 = f.apply(x);
            assert_eq!(y, y1);
        };
        run_test_for_a(0);
        run_test_for_a(77 * E8);
        run_test_for_a(888 * E8 + 123);
        run_test_for_a(9_999 * E8);

        run_test_for_b(u64_to_dec(0));
        run_test_for_b(u64_to_dec(77 * E8));
        run_test_for_b(u64_to_dec(888 * E8 + 123));
        run_test_for_b(u64_to_dec(9_999 * E8));
    }

    #[test]
    fn test_intervals() {
        let slope_denominator = 200_000 * E8;
        let max_neurons_fund_participation_icp_e8s = 95_000 * E8;
        let params = NeuronsFundParticipationConstraints {
            min_direct_participation_threshold_icp_e8s: Some(50 * E8),
            max_neurons_fund_participation_icp_e8s: Some(max_neurons_fund_participation_icp_e8s),
            coefficient_intervals: vec![
                LinearScalingCoefficient {
                    // Interval A
                    from_direct_participation_icp_e8s: Some(0),
                    to_direct_participation_icp_e8s: Some(100 * E8),
                    slope_numerator: Some(100_000 * E8),
                    slope_denominator: Some(slope_denominator),
                    intercept_icp_e8s: Some(111),
                },
                LinearScalingCoefficient {
                    // Interval B
                    from_direct_participation_icp_e8s: Some(100 * E8),
                    to_direct_participation_icp_e8s: Some(1_000 * E8),
                    slope_numerator: Some(120_000 * E8),
                    slope_denominator: Some(slope_denominator),
                    intercept_icp_e8s: Some(222),
                },
                LinearScalingCoefficient {
                    // Interval C
                    from_direct_participation_icp_e8s: Some(1_000 * E8),
                    to_direct_participation_icp_e8s: Some(10_000 * E8),
                    slope_numerator: Some(140_000 * E8),
                    slope_denominator: Some(slope_denominator),
                    intercept_icp_e8s: Some(333),
                },
                LinearScalingCoefficient {
                    // Interval D
                    from_direct_participation_icp_e8s: Some(10_000 * E8),
                    to_direct_participation_icp_e8s: Some(100_000 * E8),
                    slope_numerator: Some(160_000 * E8),
                    slope_denominator: Some(slope_denominator),
                    intercept_icp_e8s: Some(444),
                },
                LinearScalingCoefficient {
                    // Interval E
                    from_direct_participation_icp_e8s: Some(100_000 * E8),
                    to_direct_participation_icp_e8s: Some(1_000_000 * E8),
                    slope_numerator: Some(180_000 * E8),
                    slope_denominator: Some(slope_denominator),
                    intercept_icp_e8s: Some(555),
                },
            ],
        };
        let params = ValidatedNeuronsFundParticipationConstraints::try_from(params).unwrap();
        let f = SimpleLinearFunction {};
        let g: MatchedParticipationFunction =
            MatchedParticipationFunction::new(Box::from(move |x| f.apply(x)), params).unwrap();
        // Below min_direct_participation_threshold_icp_e8s
        assert_eq!(dec_to_u64(g.apply(0)).unwrap(), 0);
        // Falls into Interval A, thus we expect slope(0.5) * x + intercept_icp_e8s(111)
        assert_eq!(dec_to_u64(g.apply(90 * E8)).unwrap(), 45 * E8 + 111);
        // Falls into Interval B, thus we expect slope(0.6) * x + intercept_icp_e8s(222)
        assert_eq!(dec_to_u64(g.apply(100 * E8)).unwrap(), 60 * E8 + 222);
        // Falls into Interval C, thus we expect slope(0.7) * x + intercept_icp_e8s(333)
        assert_eq!(dec_to_u64(g.apply(5_000 * E8)).unwrap(), 3_500 * E8 + 333);
        // Falls into Interval D, thus we expect slope(0.8) * x + intercept_icp_e8s(444)
        assert_eq!(
            dec_to_u64(g.apply(100_000 * E8 - 1)).unwrap(),
            80_000 * E8 - 1 + 444
        );
        // Falls into Interval D, thus we expect slope(0.9) * x + intercept_icp_e8s(555)
        assert_eq!(
            dec_to_u64(g.apply(100_000 * E8)).unwrap(),
            90_000 * E8 + 555
        );
        // Beyond the last interval
        assert_eq!(
            dec_to_u64(g.apply(1_000_000 * E8)).unwrap(),
            max_neurons_fund_participation_icp_e8s
        );
        // Extremely high value
        assert_eq!(
            dec_to_u64(g.apply(u64::MAX)).unwrap(),
            max_neurons_fund_participation_icp_e8s
        );
    }

    const POTENTIALLY_INTERESTING_TARGET_Y_VALUES: &[&std::ops::RangeInclusive<u64>] = &[
        // The first 101 values of the the u64 range.
        &(0..=100_u64),
        // The last 101 values of the first one-third of the u64 range.
        &(6_148_914_691_236_516_764..=6_148_914_691_236_516_864),
        // The last 101 values of the u64 range.
        &(18_446_744_073_709_551_515..=u64::MAX),
    ];

    fn generate_potentially_intresting_target_values() -> Vec<u64> {
        POTENTIALLY_INTERESTING_TARGET_Y_VALUES
            .iter()
            .flat_map(|rs| {
                let rs = (*rs).clone();
                rs.collect::<Vec<u64>>()
            })
            .collect()
    }

    fn run_inverse_function_test<F>(function: &F, target_y: Decimal)
    where
        F: InvertibleFunction + AnalyticallyInvertibleFunction,
    {
        let Ok(expected) = function.invert_analytically(target_y) else {
            println!(
                "Cannot run inverse test as a u64 analytical inverse does not exist for {}.",
                target_y,
            );
            return;
        };
        let (trace, observed) = match function.invert_with_tracing(target_y) {
            (_, Err(err)) => {
                panic!("Expected inverse value, got error: {}", err);
            }
            (trace, Ok(observed)) => (trace, observed),
        };
        println!(
            "{}, target_y = {target_y} -- trace(len={}): {trace:?}",
            std::any::type_name::<F>(),
            trace.len(),
        );

        // Sometimes exact equality cannot be reached with our search strategy. We tolerate errors
        // up to 1 E8.
        assert!(
            observed.max(expected) - observed.min(expected) <= 1,
            "Deviation bigger than 1 E8.\n\
            Expected: {expected}\n\
            Observed: {observed}"
        );
    }

    #[test]
    fn test_inverse_corner_cases_with_basic_linear_function() {
        let f = SimpleLinearFunction {};
        for i in generate_potentially_intresting_target_values() {
            run_inverse_function_test(&f, u64_to_dec(i));
        }
    }

    pub struct LinearFunction {
        slope: Decimal,
        intercept: Decimal,
    }

    impl AnalyticallyInvertibleFunction for LinearFunction {
        fn invert_analytically(&self, target_y: Decimal) -> Result<u64, String> {
            if self.slope.is_zero() {
                return Err("Cannot invert constant function.".to_string());
            }
            dec_to_u64((target_y - self.intercept) / self.slope)
        }
    }

    impl InvertibleFunction for LinearFunction {
        fn apply(&self, x: u64) -> Decimal {
            let x = u64_to_dec(x);
            (x * self.slope) + self.intercept
        }
    }

    // TODO: Add tests for failing cases.

    #[test]
    fn test_inverse_corner_cases_with_slow_linear_function() {
        let slopes = vec![
            dec!(0.0001),
            dec!(0.0003),
            dec!(0.0005),
            dec!(0.001),
            dec!(0.003),
            dec!(0.005),
            dec!(0.01),
            dec!(0.03),
            dec!(0.05),
            dec!(0.1),
            dec!(0.3),
            dec!(0.5),
            dec!(1.0),
            dec!(3.0),
            dec!(5.0),
            dec!(10.0),
        ];
        let intercepts = vec![
            dec!(0.0),
            dec!(-0.0001),
            dec!(-0.0003),
            dec!(-0.0005),
            dec!(-0.001),
            dec!(-0.003),
            dec!(-0.005),
            dec!(-0.01),
            dec!(-0.03),
            dec!(-0.05),
            dec!(-0.1),
            dec!(-0.3),
            dec!(-0.5),
            dec!(-1.0),
            dec!(-3.0),
            dec!(-5.0),
            dec!(-10.0),
            dec!(-30.0),
            dec!(-50.0),
            dec!(-100.0),
            dec!(-300.0),
            dec!(-500.0),
            dec!(-1000.0),
            dec!(-3000.0),
            dec!(-5000.0),
            dec!(-10000.0),
            dec!(-30000.0),
            dec!(-50000.0),
        ];
        for intercept in intercepts {
            for slope in slopes.iter().cloned() {
                let f = LinearFunction { slope, intercept };
                for i in generate_potentially_intresting_target_values() {
                    let target_y = u64_to_dec(i);
                    println!("Inverting linear function {target_y} = f(x) = {slope} * x + {intercept} ...");
                    run_inverse_function_test(&f, target_y);
                }
            }
        }
    }
}

// -------------------------------------------------------------------------------------------------
// ------------------- NeuronsFundNeuronPortion ----------------------------------------------------
// -------------------------------------------------------------------------------------------------

/// This structure represents an arbitrary portion of a Neurons' Fund neuron, be that the whole
/// neuron (in which case `amount_icp_e8s` equals `maturity_equivalent_icp_e8s`) or a portion
/// thereof that may either participate in an SNS swap or be refunded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeuronsFundNeuronPortion {
    /// The NNS neuron ID of the participating neuron.
    pub id: NeuronId,
    /// Portion of maturity taken from this neuron. Must be less than or equal to
    /// `maturity_equivalent_icp_e8s`.
    pub amount_icp_e8s: u64,
    /// Overall amount of maturity of the neuron from which this portion is taken.
    pub maturity_equivalent_icp_e8s: u64,
    /// Controller of the neuron from which this portion is taken.
    pub controller: PrincipalId,
    /// Indicates whether the portion specified by `amount_icp_e8s` is limited due to SNS-specific
    /// participation constraints.
    pub is_capped: bool,
}

// By-default, Neurons' Fund neuron portions should be ordered lexicographically, first by
// `controller`, then by `maturity_equivalent_icp_e8s`.
impl Ord for NeuronsFundNeuronPortion {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.controller.cmp(&other.controller) {
            Ordering::Equal => self
                .maturity_equivalent_icp_e8s
                .cmp(&other.maturity_equivalent_icp_e8s),
            ordering => ordering,
        }
    }
}

impl PartialOrd for NeuronsFundNeuronPortion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
pub enum NeuronsFundNeuronPortionError {
    UnspecifiedField(String),
    AmountTooBig {
        amount_icp_e8s: u64,
        maturity_equivalent_icp_e8s: u64,
    },
}

impl ToString for NeuronsFundNeuronPortionError {
    fn to_string(&self) -> String {
        let prefix = "Invalid NeuronsFundNeuronPortion: ";
        match self {
            Self::UnspecifiedField(field_name) => {
                format!("{}field `{}` is not specified.", prefix, field_name)
            }
            Self::AmountTooBig {
                amount_icp_e8s,
                maturity_equivalent_icp_e8s,
            } => {
                format!(
                    "{}`amount_icp_e8s` ({}) exceeds `maturity_equivalent_icp_e8s` ({})",
                    prefix, amount_icp_e8s, maturity_equivalent_icp_e8s,
                )
            }
        }
    }
}

impl NeuronsFundNeuronPortionPb {
    pub fn validate(&self) -> Result<NeuronsFundNeuronPortion, NeuronsFundNeuronPortionError> {
        let id = self.nns_neuron_id.ok_or_else(|| {
            NeuronsFundNeuronPortionError::UnspecifiedField("nns_neuron_id".to_string())
        })?;
        let amount_icp_e8s = self.amount_icp_e8s.ok_or_else(|| {
            NeuronsFundNeuronPortionError::UnspecifiedField("amount_icp_e8s".to_string())
        })?;
        let maturity_equivalent_icp_e8s = self
            .maturity_equivalent_icp_e8s
            .ok_or_else(|| {
                NeuronsFundNeuronPortionError::UnspecifiedField(
                    "maturity_equivalent_icp_e8s".to_string(),
                )
            })
            .map(|maturity_equivalent_icp_e8s| {
                if maturity_equivalent_icp_e8s < amount_icp_e8s {
                    Err(NeuronsFundNeuronPortionError::AmountTooBig {
                        amount_icp_e8s,
                        maturity_equivalent_icp_e8s,
                    })
                } else {
                    Ok(maturity_equivalent_icp_e8s)
                }
            })?;
        let maturity_equivalent_icp_e8s = self.maturity_equivalent_icp_e8s.ok_or_else(|| {
            NeuronsFundNeuronPortionError::UnspecifiedField(
                "maturity_equivalent_icp_e8s".to_string(),
            )
        })?;
        let controller = self.hotkey_principal.ok_or_else(|| {
            NeuronsFundNeuronPortionError::UnspecifiedField("hotkey_principal".to_string())
        })?;
        let is_capped = self.is_capped.ok_or_else(|| {
            NeuronsFundNeuronPortionError::UnspecifiedField("is_capped".to_string())
        })?;
        Ok(NeuronsFundNeuronPortion {
            id,
            amount_icp_e8s,
            maturity_equivalent_icp_e8s,
            controller,
            is_capped,
        })
    }
}

// -------------------------------------------------------------------------------------------------
// ------------------- NeuronsFundSnapshot ---------------------------------------------------------
// -------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct NeuronsFundSnapshot {
    neurons: BTreeMap<NeuronId, NeuronsFundNeuronPortion>,
}

impl NeuronsFundSnapshot {
    pub fn empty() -> Self {
        let neurons = BTreeMap::new();
        Self { neurons }
    }

    pub fn is_empty(&self) -> bool {
        self.neurons.is_empty()
    }

    pub fn num_neurons(&self) -> usize {
        self.neurons.len()
    }

    pub fn new<I>(neurons: I) -> Self
    where
        I: IntoIterator<Item = NeuronsFundNeuronPortion>,
    {
        let neurons = neurons.into_iter().map(|n| (n.id, n)).collect();
        Self { neurons }
    }

    pub fn neurons(&self) -> &BTreeMap<NeuronId, NeuronsFundNeuronPortion> {
        &self.neurons
    }

    pub fn total_amount_icp_e8s(&self) -> u64 {
        self.neurons
            .values()
            .fold(0_u64, |a, n| a.saturating_add(n.amount_icp_e8s))
    }

    pub fn into_vec(self) -> Vec<NeuronsFundNeuronPortion> {
        self.neurons.into_values().collect()
    }

    /// Implements the `self - other` semantics for calculating Neurons' Fund refunds.
    ///
    /// Example:
    /// self = { (N1, maturity=100), (N2, maturity=200), (N3, maturity=300) }
    /// other = { (N1, maturity=60), (N3, maturity=300) }
    /// result = Ok({ (N1, maturity=40), (N2, maturity=200), (N2, maturity=200) })
    pub fn diff(&self, other: &Self) -> Result<Self, String> {
        let mut deductible_neurons = other.neurons().clone();
        let neurons = self
            .neurons
            .iter()
            .map(|(id, left)| {
                let err_prefix =
                    || format!("Cannot compute diff of two portions of neuron {:?}: ", id);
                let controller = left.controller;
                let (amount_icp_e8s, maturity_equivalent_icp_e8s, is_capped) = if let Some(right) = deductible_neurons.remove(id)
                {
                    if right.amount_icp_e8s > left.amount_icp_e8s {
                        return Err(format!(
                            "{}left.amount_icp_e8s={:?}, right.amount_icp_e8s={:?}.",
                            err_prefix(),
                            left.amount_icp_e8s,
                            right.amount_icp_e8s,
                        ));
                    }
                    if right.maturity_equivalent_icp_e8s != left.maturity_equivalent_icp_e8s {
                        return Err(format!(
                            "{}left.maturity_equivalent_icp_e8s={:?} != right.maturity_equivalent_icp_e8s={:?}.",
                            err_prefix(),
                            left.maturity_equivalent_icp_e8s,
                            right.maturity_equivalent_icp_e8s,
                        ));
                    }
                    if right.controller != controller {
                        return Err(format!(
                            "{}left.controller={:?}, right.controller={:?}.",
                            err_prefix(),
                            controller,
                            right.controller,
                        ));
                    }
                    if right.is_capped && !left.is_capped {
                        return Err(format!(
                            "{}left.is_capped=false, right.is_capped=true.",
                            err_prefix()
                        ));
                    }
                    // Taking right.is_capped, as that corresponds to the capping of the effectively
                    // taken portion of the neuron (left.is_capped is whether the originally
                    // reserved portion has been capped).
                    (left.amount_icp_e8s - right.amount_icp_e8s, left.maturity_equivalent_icp_e8s, right.is_capped)
                } else {
                    (left.amount_icp_e8s, left.maturity_equivalent_icp_e8s, left.is_capped)
                };
                Ok((
                    *id,
                    NeuronsFundNeuronPortion {
                        id: *id,
                        controller,
                        amount_icp_e8s,
                        maturity_equivalent_icp_e8s,
                        is_capped,
                    },
                ))
            })
            .collect::<Result<BTreeMap<NeuronId, NeuronsFundNeuronPortion>, _>>()?;
        if !deductible_neurons.is_empty() {
            let extra_neuron_portions_str = deductible_neurons
                .keys()
                .map(|n| n.id.to_string())
                .collect::<Vec<String>>()
                .join(", ");
            return Err(format!(
                "Cannot compute diff of two NeuronsFundSnapshot instances: right-hand side \
                contains {} extra neuron portions: {}",
                deductible_neurons.len(),
                extra_neuron_portions_str,
            ));
        }
        Ok(Self { neurons })
    }
}

impl From<NeuronsFundSnapshot> for NeuronsFundSnapshotPb {
    fn from(snapshot: NeuronsFundSnapshot) -> Self {
        let neurons_fund_neuron_portions = snapshot
            .into_vec()
            .into_iter()
            .map(Into::<NeuronsFundNeuronPortionPb>::into)
            .collect();
        Self {
            neurons_fund_neuron_portions,
        }
    }
}

#[derive(Debug)]
pub enum NeuronsFundSnapshotValidationError {
    NeuronsFundNeuronPortionError(usize, NeuronsFundNeuronPortionError),
}

impl ToString for NeuronsFundSnapshotValidationError {
    fn to_string(&self) -> String {
        let prefix = "Cannot validate NeuronsFundSnapshot: ";
        match self {
            Self::NeuronsFundNeuronPortionError(index, error) => {
                format!(
                    "{}neurons_fund_neuron_portions[{}]: {}",
                    prefix,
                    index,
                    error.to_string()
                )
            }
        }
    }
}

impl NeuronsFundSnapshotPb {
    pub fn validate(&self) -> Result<NeuronsFundSnapshot, NeuronsFundSnapshotValidationError> {
        let neurons_fund = self
            .neurons_fund_neuron_portions
            .iter()
            .enumerate()
            .map(|(i, n)| {
                n.validate().map_err(|err| {
                    NeuronsFundSnapshotValidationError::NeuronsFundNeuronPortionError(i, err)
                })
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(NeuronsFundSnapshot::new(neurons_fund.into_iter()))
    }
}

// -------------------------------------------------------------------------------------------------
// ------------------- NeuronsFundParticipation ----------------------------------------------------
// -------------------------------------------------------------------------------------------------

/// Absolute constraints of this swap needed in Matched Funding computations.
#[derive(Clone, Debug)]
pub struct SwapParticipationLimits {
    pub min_direct_participation_icp_e8s: u64,
    pub max_direct_participation_icp_e8s: u64,
    pub min_participant_icp_e8s: u64,
    pub max_participant_icp_e8s: u64,
}

#[derive(Debug)]
pub enum SwapParametersError {
    /// We expect this to never occur, and can ensure this, since the caller is Swap, and we control
    /// the code that the Swap canisters run.
    UnspecifiedField(String),
}

impl ToString for SwapParametersError {
    fn to_string(&self) -> String {
        let prefix = "Cannot extract data from SwapParameters: ";
        match self {
            Self::UnspecifiedField(field_name) => {
                format!("{}field `{}` is not specified.", prefix, field_name,)
            }
        }
    }
}

impl From<SwapParametersError> for GovernanceError {
    fn from(swap_parameters_error: SwapParametersError) -> Self {
        Self {
            error_type: governance_error::ErrorType::InvalidCommand as i32,
            error_message: swap_parameters_error.to_string(),
        }
    }
}

impl SwapParticipationLimits {
    pub fn try_from_swap_parameters(
        swap_parameters: &SwapParameters,
    ) -> Result<Self, SwapParametersError> {
        // TODO[NNS1-2608]: Support min_direct_participation_icp_e8s and max_direct_participation_icp_e8s in
        // CreateServiceNervousSystem.
        let neurons_fund_investment_icp_e8s = swap_parameters
            .neurons_fund_investment_icp
            .ok_or_else(|| {
                SwapParametersError::UnspecifiedField("neurons_fund_investment_icp".to_string())
            })?
            .e8s
            .ok_or_else(|| {
                SwapParametersError::UnspecifiedField("neurons_fund_investment_icp.e8s".to_string())
            })?;
        let min_direct_participation_icp_e8s = swap_parameters
            .minimum_icp
            .ok_or_else(|| SwapParametersError::UnspecifiedField("minimum_icp".to_string()))?
            .e8s
            .ok_or_else(|| SwapParametersError::UnspecifiedField("minimum_icp.e8s".to_string()))?
            .saturating_sub(neurons_fund_investment_icp_e8s);
        let max_direct_participation_icp_e8s = swap_parameters
            .maximum_icp
            .ok_or_else(|| SwapParametersError::UnspecifiedField("maximum_icp".to_string()))?
            .e8s
            .ok_or_else(|| SwapParametersError::UnspecifiedField("maximum_icp.e8s".to_string()))?
            .saturating_sub(neurons_fund_investment_icp_e8s);
        let min_participant_icp_e8s = swap_parameters
            .minimum_participant_icp
            .ok_or_else(|| {
                SwapParametersError::UnspecifiedField("minimum_participant_icp".to_string())
            })?
            .e8s
            .ok_or_else(|| {
                SwapParametersError::UnspecifiedField("minimum_participant_icp.e8s".to_string())
            })?;
        let max_participant_icp_e8s = swap_parameters
            .maximum_participant_icp
            .ok_or_else(|| {
                SwapParametersError::UnspecifiedField("maximum_participant_icp".to_string())
            })?
            .e8s
            .ok_or_else(|| {
                SwapParametersError::UnspecifiedField("maximum_participant_icp.e8s".to_string())
            })?;
        Ok(Self {
            min_direct_participation_icp_e8s,
            max_direct_participation_icp_e8s,
            min_participant_icp_e8s,
            max_participant_icp_e8s,
        })
    }
}

/// Information for deciding how the Neurons' Fund should participate in an SNS Swap.
#[derive(Debug)]
pub struct NeuronsFundParticipation {
    swap_participation_limits: SwapParticipationLimits,
    ideal_matched_participation_function: Box<dyn IdealMatchingFunction>,
    /// Represents the participation amount per Neurons' Fund neuron.
    neurons_fund_reserves: NeuronsFundSnapshot,
    /// Neurons' Fund participation is computed for this amount of direct participation.
    direct_participation_icp_e8s: u64,
    /// Total amount of maturity in the Neurons' Fund at the time when the Neurons' Fund
    /// participation was created.
    total_maturity_equivalent_icp_e8s: u64,
    /// Maximum amount that the Neurons' Fund will participate with in this SNS swap, regardless of
    /// how large the value of `direct_participation_icp_e8s` is. This value is capped by whichever
    /// of the three is the smallest value:
    /// * `ideal_matched_participation_function.apply(swap_participation_limits.max_direct_participation_icp_e8s)`,
    /// * `MAX_THEORETICAL_NEURONS_FUND_PARTICIPATION_AMOUNT_ICP_E8S`,
    /// * 10% of the total Neurons' Fund maturity ICP equivalent.
    ///
    /// Warning: This value does not take into account limiting the participation of individual
    /// Neurons' Fund neurons, i.e., capping and dropping. To compute the precise Neurons' Fund
    /// participation amount, use `neurons_fund_reserves.total_amount_icp_e8s()`.
    max_neurons_fund_swap_participation_icp_e8s: u64,
    /// How much the Neurons' Fund would ideally like to participate with in this SNS swap,
    /// given the direct participation amount (`direct_participation_icp_e8s`) and matching function
    /// (`ideal_matched_participation_function`).
    ///
    /// Warning: This value does not take into account limiting the participation of individual
    /// Neurons' Fund neurons, i.e., capping and dropping. To compute the precise Neurons' Fund
    /// participation amount, use `neurons_fund_reserves.total_amount_icp_e8s()`.
    intended_neurons_fund_participation_icp_e8s: u64,
}

impl NeuronsFundParticipation {
    /// Returns whether there is some participation at all.
    pub fn is_empty(&self) -> bool {
        self.neurons_fund_reserves.is_empty()
    }

    /// Returns the total Neurons' Fund participation amount.
    pub fn total_amount_icp_e8s(&self) -> u64 {
        self.neurons_fund_reserves.total_amount_icp_e8s()
    }

    pub fn num_neurons(&self) -> usize {
        self.neurons_fund_reserves.num_neurons()
    }

    /// Create a new Neurons' Fund participation for the given `swap_participation_limits`.
    pub fn new(
        swap_participation_limits: SwapParticipationLimits,
        neurons_fund: Vec<NeuronsFundNeuron>,
        ideal_matched_participation_function: Box<dyn IdealMatchingFunction>,
    ) -> Result<Self, String> {
        let total_maturity_equivalent_icp_e8s = neurons_fund
            .iter()
            .map(|neuron| neuron.maturity_equivalent_icp_e8s)
            .fold(0_u64, |a, n| a.saturating_add(n));
        Self::new_impl(
            total_maturity_equivalent_icp_e8s,
            swap_participation_limits.max_direct_participation_icp_e8s,
            swap_participation_limits,
            neurons_fund,
            ideal_matched_participation_function,
        )
    }

    /// Consumes self, returning the contained `NeuronsFundSnapshot`.
    pub fn into_snapshot(self) -> NeuronsFundSnapshot {
        self.neurons_fund_reserves
    }

    /// Borrows self, returning a reference to the contained `NeuronsFundSnapshot`.
    pub fn snapshot(&self) -> &NeuronsFundSnapshot {
        &self.neurons_fund_reserves
    }

    /// Retains self, returning a cloned version of the contained `NeuronsFundSnapshot`.
    pub fn snapshot_cloned(&self) -> NeuronsFundSnapshot {
        self.neurons_fund_reserves.clone()
    }

    /// Create a new Neurons' Fund participation matching given `direct_participation_icp_e8s`.
    /// All other parameters are taken from `self`.
    pub fn from_initial_participation(
        &self,
        direct_participation_icp_e8s: u64,
    ) -> Result<Self, String> {
        let swap_participation_limits = self.swap_participation_limits.clone();
        let total_maturity_equivalent_icp_e8s = self.total_maturity_equivalent_icp_e8s;
        let neurons_fund = self
            .snapshot()
            .neurons()
            .values()
            .map(
                |NeuronsFundNeuronPortion {
                     id,
                     maturity_equivalent_icp_e8s,
                     controller,
                     ..
                 }| {
                    NeuronsFundNeuron {
                        id: *id,
                        maturity_equivalent_icp_e8s: *maturity_equivalent_icp_e8s,
                        controller: *controller,
                    }
                },
            )
            .collect();
        let ideal_matched_participation_function = {
            // Work around the fact that we cannot call `.clone()` on this type.
            let repr = self.ideal_matched_participation_function.serialize();
            Box::from(SimpleLinearFunction::new(&repr)?)
        };
        Self::new_impl(
            total_maturity_equivalent_icp_e8s,
            direct_participation_icp_e8s,
            swap_participation_limits,
            neurons_fund,
            ideal_matched_participation_function,
        )
    }

    fn new_impl(
        total_maturity_equivalent_icp_e8s: u64,
        direct_participation_icp_e8s: u64,
        swap_participation_limits: SwapParticipationLimits,
        neurons_fund: Vec<NeuronsFundNeuron>,
        ideal_matched_participation_function: Box<dyn IdealMatchingFunction>,
    ) -> Result<Self, String> {
        // Take 10% of overall Neurons' Fund maturity.
        let max_neurons_fund_swap_participation_icp_e8s =
            take_max_initial_neurons_fund_participation_percentage(
                total_maturity_equivalent_icp_e8s,
            );
        // Apply hard cap.
        let max_neurons_fund_swap_participation_icp_e8s = u64::min(
            max_neurons_fund_swap_participation_icp_e8s,
            MAX_THEORETICAL_NEURONS_FUND_PARTICIPATION_AMOUNT_ICP_E8S,
        );
        // Apply cap dictated by `ideal_matched_participation_function`.
        let max_neurons_fund_swap_participation_icp_e8s = Decimal::min(
            u64_to_dec(max_neurons_fund_swap_participation_icp_e8s),
            ideal_matched_participation_function
                .apply(swap_participation_limits.max_direct_participation_icp_e8s),
        );
        let intended_neurons_fund_participation_icp_e8s = Decimal::min(
            ideal_matched_participation_function.apply(direct_participation_icp_e8s),
            max_neurons_fund_swap_participation_icp_e8s,
        );
        let neurons_fund_reserves =
            NeuronsFundSnapshot::new(neurons_fund.into_iter().filter_map(
                |NeuronsFundNeuron {
                     id,
                     maturity_equivalent_icp_e8s,
                     controller,
                 }| {
                    let proportion_to_overall_neurons_fund: Decimal = u64_to_dec(maturity_equivalent_icp_e8s)
                        / u64_to_dec(total_maturity_equivalent_icp_e8s);
                    let ideal_participation_amount_icp_e8s: u64 =
                        match dec_to_u64(proportion_to_overall_neurons_fund * intended_neurons_fund_participation_icp_e8s) {
                            Ok(ideal_participation_amount_icp_e8s) => {
                                ideal_participation_amount_icp_e8s
                            }
                            Err(err) => {
                                // This cannot practically happen as `dec_to_u64` returns an error
                                // only in two cases: (1) the argument is negative (we've multiplied
                                // two non-negative numbers, `proportion_to_overall_neurons_fund`
                                // and `intended_neurons_fund_participation_icp_e8s`) and (2) there
                                // is a u64 overflow (`intended_neurons_fund_participation_icp_e8s`
                                // is bounded by `u64::MAX` and `proportion_to_overall_neurons_fund`
                                // is a value between 0.0 and 1.0). If these assumptions are somehow
                                // still violated, we log this situation to aid debugging.
                                println!(
                                    "{}ERROR: Cannot compute ideal participation amount for \
                                    Neurons' Fund neuron {:?}: {}",
                                    governance::LOG_PREFIX, id, err,
                                );
                                return None;
                            }
                        };
                    if ideal_participation_amount_icp_e8s < swap_participation_limits.min_participant_icp_e8s {
                        // Do not include neurons that cannot participate under any circumstances.
                        println!(
                            "{}INFO: discarding neuron {:?} ({} ICP e8s maturity equivalent) as it \
                            cannot participate in the swap with its proportional participation \
                            amount ({}) that is less than `min_participant_icp_e8s` ({}).",
                            governance::LOG_PREFIX, id, maturity_equivalent_icp_e8s,
                            ideal_participation_amount_icp_e8s,
                            swap_participation_limits.min_participant_icp_e8s,
                        );
                        None
                    } else {
                        let (amount_icp_e8s, is_capped) = if ideal_participation_amount_icp_e8s > swap_participation_limits.max_participant_icp_e8s {
                            println!(
                                "{}INFO: capping neuron {:?} ({} ICP e8s maturity equivalent) as it \
                                cannot participate in the swap with all of its proportional \
                                participation amount ({}) that exceeds `max_participant_icp_e8s` ({}).",
                                governance::LOG_PREFIX, id, maturity_equivalent_icp_e8s,
                                ideal_participation_amount_icp_e8s,
                                swap_participation_limits.max_participant_icp_e8s,
                            );
                            (swap_participation_limits.max_participant_icp_e8s, true)
                        } else {
                            (ideal_participation_amount_icp_e8s, false)
                        };
                        Some(NeuronsFundNeuronPortion {
                            id,
                            amount_icp_e8s,
                            maturity_equivalent_icp_e8s,
                            controller,
                            is_capped,
                        })
                    }
                },
            ));

        // Store the values in `u64` to simplify serializing. This should be okay, as rounding
        // errors here will not impede auditability (the main reason we store them).
        let intended_neurons_fund_participation_icp_e8s =
            dec_to_u64(intended_neurons_fund_participation_icp_e8s)?;
        let max_neurons_fund_swap_participation_icp_e8s =
            dec_to_u64(max_neurons_fund_swap_participation_icp_e8s)?;
        Ok(Self {
            swap_participation_limits,
            ideal_matched_participation_function,
            neurons_fund_reserves,
            direct_participation_icp_e8s,
            total_maturity_equivalent_icp_e8s,
            max_neurons_fund_swap_participation_icp_e8s,
            intended_neurons_fund_participation_icp_e8s,
        })
    }

    /// TODO[NNS1-2591]: Implement the rest of this function. Currently, it returns a mock structure
    /// that will pass validiation but does not reflect the real Neurons' Fund participation.
    /// After this TODO is addressed, the tests in rs/nns/governance/tests/governance.rs would need
    /// to be adjusted.
    pub fn compute_constraints(&self) -> Result<NeuronsFundParticipationConstraints, String> {
        let min_direct_participation_threshold_icp_e8s = Some(
            self.swap_participation_limits
                .min_direct_participation_icp_e8s,
        );
        let max_neurons_fund_participation_icp_e8s =
            Some(self.max_neurons_fund_swap_participation_icp_e8s);
        let dummy_interval = ValidatedLinearScalingCoefficient {
            from_direct_participation_icp_e8s: 0,
            to_direct_participation_icp_e8s: self.max_neurons_fund_swap_participation_icp_e8s,
            slope_numerator: 1,
            slope_denominator: 1,
            intercept_icp_e8s: 0,
        };
        let dummy_interval: LinearScalingCoefficient = dummy_interval.into();
        let coefficient_intervals = vec![dummy_interval];
        Ok(NeuronsFundParticipationConstraints {
            min_direct_participation_threshold_icp_e8s,
            max_neurons_fund_participation_icp_e8s,
            coefficient_intervals,
        })
    }
}

#[derive(Debug)]
pub enum NeuronsFundParticipationValidationError {
    UnspecifiedField(String),
    NeuronsFundSnapshotValidationError(NeuronsFundSnapshotValidationError),
    MatchFunctionDeserializationFailed(String),
    NeuronsFundParticipationCreationFailed(String),
}

impl ToString for NeuronsFundParticipationValidationError {
    fn to_string(&self) -> String {
        let prefix = "Cannot validate NeuronsFundParticipation: ";
        match self {
            Self::UnspecifiedField(field_name) => {
                format!("{}field `{}` is not specified.", prefix, field_name)
            }
            Self::NeuronsFundSnapshotValidationError(error) => {
                format!("{}{}", prefix, error.to_string())
            }
            Self::MatchFunctionDeserializationFailed(error) => {
                format!(
                    "{}failed to deserialize an IdealMatchingFunction instance: {}",
                    prefix, error
                )
            }
            Self::NeuronsFundParticipationCreationFailed(error) => {
                format!(
                    "{}failed to create NeuronsFundParticipation: {}",
                    prefix, error
                )
            }
        }
    }
}

impl From<NeuronsFundParticipation> for NeuronsFundParticipationPb {
    fn from(participation: NeuronsFundParticipation) -> Self {
        let serialized_representation = Some(
            participation
                .ideal_matched_participation_function
                .serialize(),
        );
        let ideal_matched_participation_function = Some(IdealMatchedParticipationFunction {
            serialized_representation,
        });
        let swap_participation_limits = Some(SwapParticipationLimitsPb {
            min_direct_participation_icp_e8s: Some(
                participation
                    .swap_participation_limits
                    .min_direct_participation_icp_e8s,
            ),
            max_direct_participation_icp_e8s: Some(
                participation
                    .swap_participation_limits
                    .max_direct_participation_icp_e8s,
            ),
            min_participant_icp_e8s: Some(
                participation
                    .swap_participation_limits
                    .min_participant_icp_e8s,
            ),
            max_participant_icp_e8s: Some(
                participation
                    .swap_participation_limits
                    .max_participant_icp_e8s,
            ),
        });
        let direct_participation_icp_e8s = Some(participation.direct_participation_icp_e8s);
        let total_maturity_equivalent_icp_e8s =
            Some(participation.total_maturity_equivalent_icp_e8s);
        let max_neurons_fund_swap_participation_icp_e8s =
            Some(participation.max_neurons_fund_swap_participation_icp_e8s);
        let intended_neurons_fund_participation_icp_e8s =
            Some(participation.intended_neurons_fund_participation_icp_e8s);
        let neurons_fund_neuron_portions: Vec<NeuronsFundNeuronPortionPb> = participation
            .into_snapshot()
            .neurons()
            .values()
            .map(|neuron| NeuronsFundNeuronPortionPb {
                nns_neuron_id: Some(neuron.id),
                amount_icp_e8s: Some(neuron.amount_icp_e8s),
                maturity_equivalent_icp_e8s: Some(neuron.maturity_equivalent_icp_e8s),
                hotkey_principal: Some(neuron.controller),
                is_capped: Some(neuron.is_capped),
            })
            .collect();
        let neurons_fund_reserves = Some(NeuronsFundSnapshotPb {
            neurons_fund_neuron_portions,
        });
        Self {
            ideal_matched_participation_function,
            neurons_fund_reserves,
            swap_participation_limits,
            direct_participation_icp_e8s,
            total_maturity_equivalent_icp_e8s,
            max_neurons_fund_swap_participation_icp_e8s,
            intended_neurons_fund_participation_icp_e8s,
        }
    }
}

impl NeuronsFundParticipationPb {
    /// Validate that a NeuronsFundParticipationPb structure is free of defects, returning a
    /// NeuronsFundParticipation structure with validated fields.
    pub fn validate(
        &self,
    ) -> Result<NeuronsFundParticipation, NeuronsFundParticipationValidationError> {
        let ideal_match_function_repr = self
            .ideal_matched_participation_function
            .as_ref()
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "ideal_matched_participation_function".to_string(),
                )
            })?
            .serialized_representation
            .as_ref()
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "ideal_matched_participation_function.serialized_representation".to_string(),
                )
            })?;
        let ideal_matched_participation_function: Box<dyn IdealMatchingFunction> = Box::from(
            SimpleLinearFunction::new(ideal_match_function_repr).map_err(
                NeuronsFundParticipationValidationError::MatchFunctionDeserializationFailed,
            )?,
        );
        let neurons_fund_reserves = self
            .neurons_fund_reserves
            .as_ref()
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "neurons_fund_reserves".to_string(),
                )
            })?
            .validate()
            .map_err(NeuronsFundParticipationValidationError::NeuronsFundSnapshotValidationError)?;
        let swap_participation_limits =
            self.swap_participation_limits.as_ref().ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "swap_participation_limits".to_string(),
                )
            })?;
        let min_direct_participation_icp_e8s = swap_participation_limits
            .min_direct_participation_icp_e8s
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "swap_participation_limits.min_direct_participation_icp_e8s".to_string(),
                )
            })?;
        let max_direct_participation_icp_e8s = swap_participation_limits
            .max_direct_participation_icp_e8s
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "swap_participation_limits.max_direct_participation_icp_e8s".to_string(),
                )
            })?;
        let min_participant_icp_e8s = swap_participation_limits
            .min_participant_icp_e8s
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "swap_participation_limits.min_participant_icp_e8s".to_string(),
                )
            })?;
        let max_participant_icp_e8s = swap_participation_limits
            .max_participant_icp_e8s
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "swap_participation_limits.max_participant_icp_e8s".to_string(),
                )
            })?;
        let swap_participation_limits = SwapParticipationLimits {
            min_direct_participation_icp_e8s,
            max_direct_participation_icp_e8s,
            min_participant_icp_e8s,
            max_participant_icp_e8s,
        };
        let direct_participation_icp_e8s = self.direct_participation_icp_e8s.ok_or_else(|| {
            NeuronsFundParticipationValidationError::UnspecifiedField(
                "direct_participation_icp_e8s".to_string(),
            )
        })?;
        let total_maturity_equivalent_icp_e8s =
            self.total_maturity_equivalent_icp_e8s.ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "total_maturity_equivalent_icp_e8s".to_string(),
                )
            })?;
        let max_neurons_fund_swap_participation_icp_e8s = self
            .max_neurons_fund_swap_participation_icp_e8s
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "max_neurons_fund_swap_participation_icp_e8s".to_string(),
                )
            })?;
        let intended_neurons_fund_participation_icp_e8s = self
            .intended_neurons_fund_participation_icp_e8s
            .ok_or_else(|| {
                NeuronsFundParticipationValidationError::UnspecifiedField(
                    "intended_neurons_fund_participation_icp_e8s".to_string(),
                )
            })?;
        Ok(NeuronsFundParticipation {
            swap_participation_limits,
            ideal_matched_participation_function,
            neurons_fund_reserves,
            direct_participation_icp_e8s,
            total_maturity_equivalent_icp_e8s,
            max_neurons_fund_swap_participation_icp_e8s,
            intended_neurons_fund_participation_icp_e8s,
        })
    }
}

// -------------------------------------------------------------------------------------------------
// ------------------- NeuronsFundAction -----------------------------------------------------------
// -------------------------------------------------------------------------------------------------

pub enum NeuronsFundAction {
    DrawMaturity,
    RefundMaturity,
}

impl NeuronsFundAction {
    pub fn checked_apply(&self, left: u64, right: u64) -> Result<u64, String> {
        match self {
            Self::DrawMaturity => left.checked_sub(right).ok_or_else(|| "drawing".to_string()),
            Self::RefundMaturity => left
                .checked_add(right)
                .ok_or_else(|| "refunding".to_string()),
        }
    }
}

/// Apply the Neurons' Fund snapshot, i.e., either (depending on `action`) add or subtract maturity
/// to Neurons' Fund neurons stored in `neuron_store`.
///
/// Potential refund errors (e.g., u64 overflows) are collected, serialized, and returned as
/// the Err result. Note that the maturity of neurons for which thean error occured does not
/// need to be adjusted, as the function will retain their original maturity in case of errors.
fn apply_neurons_fund_snapshot(
    neuron_store: &mut NeuronStore,
    snapshot: &NeuronsFundSnapshot,
    action: NeuronsFundAction,
) -> Result<(), String> {
    let mut neurons_fund_action_error = vec![];
    for (neuron_id, neuron_delta) in snapshot.neurons().iter() {
        let refund_result = neuron_store.with_neuron_mut(neuron_id, |nns_neuron| {
            let old_nns_neuron_maturity_e8s = nns_neuron.maturity_e8s_equivalent;
            let maturity_delta_e8s = neuron_delta.amount_icp_e8s;
            nns_neuron.maturity_e8s_equivalent = action
                .checked_apply(old_nns_neuron_maturity_e8s, maturity_delta_e8s)
                .unwrap_or_else(|verb| {
                    neurons_fund_action_error.push(format!(
                        "u64 overflow while {verb} maturity from {neuron_id:?} \
                            (*kept* original maturity e8s = {old_nns_neuron_maturity_e8s}; \
                            requested maturity delta e8s = {maturity_delta_e8s})."
                    ));
                    old_nns_neuron_maturity_e8s
                });
        });
        if let Err(with_neuron_mut_error) = refund_result {
            neurons_fund_action_error.push(with_neuron_mut_error.to_string());
        }
    }
    if neurons_fund_action_error.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Errors while mutating the Neurons' Fund:\n  - {}",
            neurons_fund_action_error.join("\n  - ")
        ))
    }
}

pub trait NeuronsFund {
    fn draw_maturity_from_neurons_fund(
        &mut self,
        snapshot: &NeuronsFundSnapshot,
    ) -> Result<(), String>;

    fn refund_maturity_to_neurons_fund(
        &mut self,
        snapshot: &NeuronsFundSnapshot,
    ) -> Result<(), String>;
}

impl NeuronsFund for NeuronStore {
    fn draw_maturity_from_neurons_fund(
        &mut self,
        snapshot: &NeuronsFundSnapshot,
    ) -> Result<(), String> {
        apply_neurons_fund_snapshot(self, snapshot, NeuronsFundAction::DrawMaturity)
    }

    fn refund_maturity_to_neurons_fund(
        &mut self,
        snapshot: &NeuronsFundSnapshot,
    ) -> Result<(), String> {
        apply_neurons_fund_snapshot(self, snapshot, NeuronsFundAction::RefundMaturity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use lazy_static::lazy_static;

    use ic_nervous_system_common::E8;
    use rust_decimal_macros::dec;

    use ic_nns_common::pb::v1::NeuronId;

    const SWAP_LIMITS: SwapParticipationLimits = SwapParticipationLimits {
        min_direct_participation_icp_e8s: 75_000 * E8,
        max_direct_participation_icp_e8s: 300_000 * E8,
        min_participant_icp_e8s: 10 * E8,
        max_participant_icp_e8s: 50_000 * E8,
    };

    const N1: NeuronId = NeuronId { id: 11 };
    const N2: NeuronId = NeuronId { id: 12 };
    const N3: NeuronId = NeuronId { id: 13 };

    lazy_static! {
        static ref PRINCIPAL_ID_1: PrincipalId = PrincipalId::new_user_test_id(1);
        static ref PRINCIPAL_ID_2: PrincipalId = PrincipalId::new_user_test_id(2);
        static ref PRINCIPAL_ID_3: PrincipalId = PrincipalId::new_user_test_id(3);
    }

    #[test]
    fn test_neurons_fund_participation_with_no_neurons() {
        let initial_neurons_fund_participation =
            NeuronsFundParticipation::new(SWAP_LIMITS, vec![], Box::new(SimpleLinearFunction {}));
        let initial_neurons_fund_participation = initial_neurons_fund_participation.unwrap();
        let final_neurons_fund_participation = initial_neurons_fund_participation
            .from_initial_participation(SWAP_LIMITS.min_direct_participation_icp_e8s)
            .unwrap();
        // I. Validate the public API
        assert!(initial_neurons_fund_participation.is_empty());
        assert_eq!(initial_neurons_fund_participation.num_neurons(), 0);
        assert_eq!(initial_neurons_fund_participation.total_amount_icp_e8s(), 0);
        assert!(final_neurons_fund_participation.is_empty());
        assert_eq!(final_neurons_fund_participation.num_neurons(), 0);
        assert_eq!(final_neurons_fund_participation.total_amount_icp_e8s(), 0);

        // II. Validate the private state
        assert_matches!(
            initial_neurons_fund_participation,
            NeuronsFundParticipation {
                neurons_fund_reserves,
                direct_participation_icp_e8s,
                total_maturity_equivalent_icp_e8s,
                intended_neurons_fund_participation_icp_e8s,
                max_neurons_fund_swap_participation_icp_e8s,
                ..
            } => {
                assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::empty());
                assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.max_direct_participation_icp_e8s);
                assert_eq!(total_maturity_equivalent_icp_e8s, 0);
                assert_eq!(intended_neurons_fund_participation_icp_e8s, 0);
                assert_eq!(max_neurons_fund_swap_participation_icp_e8s, 0);
            }
        );
        assert_matches!(
            final_neurons_fund_participation,
            NeuronsFundParticipation {
                neurons_fund_reserves,
                direct_participation_icp_e8s,
                total_maturity_equivalent_icp_e8s,
                intended_neurons_fund_participation_icp_e8s,
                max_neurons_fund_swap_participation_icp_e8s,
                ..
            } => {
                assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::empty());
                assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                assert_eq!(total_maturity_equivalent_icp_e8s, 0);
                assert_eq!(intended_neurons_fund_participation_icp_e8s, 0);
                assert_eq!(max_neurons_fund_swap_participation_icp_e8s, 0);
            }
        );
    }

    #[test]
    fn test_neurons_fund_participation_with_one_mid_size_neuron() {
        println!("{:?}", SWAP_LIMITS);
        let neurons_fund = vec![NeuronsFundNeuron {
            id: N1,
            maturity_equivalent_icp_e8s: 500 * E8,
            controller: *PRINCIPAL_ID_1,
        }];
        let initial_neurons_fund_participation = NeuronsFundParticipation::new(
            SWAP_LIMITS,
            neurons_fund.clone(),
            Box::new(SimpleLinearFunction {}),
        )
        .unwrap();
        let final_neurons_fund_participation = initial_neurons_fund_participation
            .from_initial_participation(SWAP_LIMITS.min_direct_participation_icp_e8s)
            .unwrap();
        // I. Validate the public API
        assert!(!initial_neurons_fund_participation.is_empty());
        assert_eq!(initial_neurons_fund_participation.num_neurons(), 1);
        assert_eq!(
            initial_neurons_fund_participation.total_amount_icp_e8s(),
            50 * E8
        );
        assert!(!final_neurons_fund_participation.is_empty());
        assert_eq!(final_neurons_fund_participation.num_neurons(), 1);
        // Despite the decreased participation amount (from max_direct_participation_icp_e8s to
        // min_direct_participation_icp_e8s), the total amount of ICP e8s taken from the NF is
        // capped at `max_neurons_fund_swap_participation_icp_e8s`, i.e., 10% of the NF.
        assert_eq!(
            final_neurons_fund_participation.total_amount_icp_e8s(),
            50 * E8
        );
        // II. Validate the private state
        assert_matches!(
            initial_neurons_fund_participation,
            NeuronsFundParticipation {
                neurons_fund_reserves,
                direct_participation_icp_e8s,
                total_maturity_equivalent_icp_e8s,
                intended_neurons_fund_participation_icp_e8s,
                max_neurons_fund_swap_participation_icp_e8s,
                ..
            } => {
                assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                    vec![
                        NeuronsFundNeuronPortion {
                            id: N1,
                            amount_icp_e8s: 50 * E8, // 10% from the NF
                            maturity_equivalent_icp_e8s: 500 * E8,
                            controller: *PRINCIPAL_ID_1,
                            is_capped: false,
                        },
                    ]
                ));
                assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.max_direct_participation_icp_e8s);
                assert_eq!(total_maturity_equivalent_icp_e8s, 500 * E8);
                assert_eq!(intended_neurons_fund_participation_icp_e8s, 50 * E8);
                assert_eq!(max_neurons_fund_swap_participation_icp_e8s, 50 * E8);
            }
        );
        assert_matches!(
            final_neurons_fund_participation,
            NeuronsFundParticipation {
                neurons_fund_reserves,
                direct_participation_icp_e8s,
                total_maturity_equivalent_icp_e8s,
                intended_neurons_fund_participation_icp_e8s,
                max_neurons_fund_swap_participation_icp_e8s,
                ..
            } => {
                assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                    vec![
                        NeuronsFundNeuronPortion {
                            id: N1,
                            amount_icp_e8s: 50 * E8, // 10% from the NF
                            maturity_equivalent_icp_e8s: 500 * E8,
                            controller: *PRINCIPAL_ID_1,
                            is_capped: false,
                        },
                    ]
                ));
                assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                assert_eq!(total_maturity_equivalent_icp_e8s, 500 * E8);
                assert_eq!(intended_neurons_fund_participation_icp_e8s, 50 * E8);
                assert_eq!(max_neurons_fund_swap_participation_icp_e8s, 50 * E8);
            }
        );
    }

    fn total_nf_maturity_icp_e8s(nf: &[NeuronsFundNeuron]) -> u64 {
        nf.iter().fold(0, |a, n| a + n.maturity_equivalent_icp_e8s)
    }

    #[test]
    fn test_neurons_fund_participation_with_one_tiny_neuron() {
        // minimal_sufficient_maturity_icp_e8s = (min_participant_icp_e8s / overall_initial_neurons_fund_participation) * total_NF_maturity
        // min_participant_icp_e8s = SWAP_LIMITS.min_participant_icp_e8s
        // overall_initial_neurons_fund_participation = 10% of total_NF_maturity
        let minimal_sufficient_maturity_icp_e8s = 10 * SWAP_LIMITS.min_participant_icp_e8s;
        // Test case A: one NF neuron with slightly insufficient amount of maturity, so it is
        // ineligible to participate.
        {
            let nf = vec![NeuronsFundNeuron {
                id: N1,
                maturity_equivalent_icp_e8s: minimal_sufficient_maturity_icp_e8s - 1,
                controller: *PRINCIPAL_ID_1,
            }];
            let initial_neurons_fund_participation = NeuronsFundParticipation::new(
                SWAP_LIMITS,
                nf.clone(),
                Box::new(SimpleLinearFunction {}),
            )
            .unwrap();
            let final_neurons_fund_participation = initial_neurons_fund_participation
                .from_initial_participation(SWAP_LIMITS.min_direct_participation_icp_e8s)
                .unwrap();

            // I. Validate the public API
            assert!(initial_neurons_fund_participation.is_empty());
            assert_eq!(initial_neurons_fund_participation.num_neurons(), 0);
            assert_eq!(initial_neurons_fund_participation.total_amount_icp_e8s(), 0);
            // Since we already had insufficient maturity, it should stay that way at the end.
            assert!(final_neurons_fund_participation.is_empty());
            assert_eq!(final_neurons_fund_participation.num_neurons(), 0);
            assert_eq!(final_neurons_fund_participation.total_amount_icp_e8s(), 0);

            // II. Validate the private state
            assert_matches!(
                initial_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::empty());
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.max_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, minimal_sufficient_maturity_icp_e8s - 1);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
            assert_matches!(
                final_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::empty());
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, minimal_sufficient_maturity_icp_e8s - 1);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
        }
        // Test case B: one NF neuron with just enough maturity, so it is eligible to participate.
        {
            let nf = vec![NeuronsFundNeuron {
                id: N1,
                maturity_equivalent_icp_e8s: minimal_sufficient_maturity_icp_e8s,
                controller: *PRINCIPAL_ID_1,
            }];
            let initial_neurons_fund_participation = NeuronsFundParticipation::new(
                SWAP_LIMITS,
                nf.clone(),
                Box::new(SimpleLinearFunction {}),
            )
            .unwrap();
            let final_neurons_fund_participation = initial_neurons_fund_participation
                .from_initial_participation(SWAP_LIMITS.min_direct_participation_icp_e8s)
                .unwrap();
            // I. Validate the public API
            assert!(!initial_neurons_fund_participation.is_empty());
            assert_eq!(initial_neurons_fund_participation.num_neurons(), 1);
            assert_eq!(
                initial_neurons_fund_participation.total_amount_icp_e8s(),
                take_max_initial_neurons_fund_participation_percentage(
                    minimal_sufficient_maturity_icp_e8s
                )
            );
            assert!(!final_neurons_fund_participation.is_empty());
            assert_eq!(final_neurons_fund_participation.num_neurons(), 1);
            // Since the entire NF consists of one neuron, we reach the 10% cap of the NF
            // participation (both before and after and swap). So `total_amount_icp_e8s` = const.
            assert_eq!(
                final_neurons_fund_participation.total_amount_icp_e8s(),
                take_max_initial_neurons_fund_participation_percentage(
                    minimal_sufficient_maturity_icp_e8s
                )
            );

            // II. Validate the private state
            assert_matches!(
                initial_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N1,
                                amount_icp_e8s: take_max_initial_neurons_fund_participation_percentage(minimal_sufficient_maturity_icp_e8s),
                                maturity_equivalent_icp_e8s: minimal_sufficient_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_1,
                                is_capped: false,
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.max_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, minimal_sufficient_maturity_icp_e8s);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
            assert_matches!(
                final_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N1,
                                amount_icp_e8s: take_max_initial_neurons_fund_participation_percentage(minimal_sufficient_maturity_icp_e8s),
                                maturity_equivalent_icp_e8s: minimal_sufficient_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_1,
                                is_capped: false,
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, minimal_sufficient_maturity_icp_e8s);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
        }
    }

    #[test]
    fn test_neurons_fund_participation_with_one_giant_neuron() {
        // maximum_uncapped_maturity_icp_e8s = (max_participant_icp_e8s / overall_initial_neurons_fund_participation) * total_NF_maturity
        // max_participant_icp_e8s = SWAP_LIMITS.max_participant_icp_e8s
        // overall_initial_neurons_fund_participation = 10% of total_NF_maturity
        let maximum_uncapped_maturity_icp_e8s = 10 * SWAP_LIMITS.max_participant_icp_e8s;
        let minimum_capped_maturity_icp_e8s = 10 * (1 + SWAP_LIMITS.max_participant_icp_e8s);
        // Test case A: one NF neuron with maximum maturity, so it is eligible to participate.
        {
            let nf = vec![NeuronsFundNeuron {
                id: N1,
                maturity_equivalent_icp_e8s: maximum_uncapped_maturity_icp_e8s,
                controller: *PRINCIPAL_ID_1,
            }];
            let initial_neurons_fund_participation = NeuronsFundParticipation::new(
                SWAP_LIMITS,
                nf.clone(),
                Box::new(SimpleLinearFunction {}),
            )
            .unwrap();
            let final_neurons_fund_participation = initial_neurons_fund_participation
                .from_initial_participation(SWAP_LIMITS.min_direct_participation_icp_e8s)
                .unwrap();
            // I. Validate the public API
            assert!(!initial_neurons_fund_participation.is_empty());
            assert_eq!(initial_neurons_fund_participation.num_neurons(), 1);
            assert_eq!(
                initial_neurons_fund_participation.total_amount_icp_e8s(),
                take_max_initial_neurons_fund_participation_percentage(
                    maximum_uncapped_maturity_icp_e8s
                )
            );
            assert!(!final_neurons_fund_participation.is_empty());
            assert_eq!(final_neurons_fund_participation.num_neurons(), 1);
            // Since the entire NF consists of one neuron, we reach the 10% cap of the NF
            // participation (both before and after and swap). So `total_amount_icp_e8s` = const.
            assert_eq!(
                final_neurons_fund_participation.total_amount_icp_e8s(),
                take_max_initial_neurons_fund_participation_percentage(
                    maximum_uncapped_maturity_icp_e8s
                )
            );

            // II. Validate the private state
            assert_matches!(
                initial_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N1,
                                amount_icp_e8s: take_max_initial_neurons_fund_participation_percentage(maximum_uncapped_maturity_icp_e8s),
                                maturity_equivalent_icp_e8s: maximum_uncapped_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_1,
                                is_capped: false, // note this is uncapped
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.max_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, maximum_uncapped_maturity_icp_e8s);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
            assert_matches!(
                final_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N1,
                                amount_icp_e8s: take_max_initial_neurons_fund_participation_percentage(maximum_uncapped_maturity_icp_e8s),
                                maturity_equivalent_icp_e8s: maximum_uncapped_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_1,
                                is_capped: false, // note this is uncapped
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, maximum_uncapped_maturity_icp_e8s);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
        }
        // Test case B: one NF neuron with just over maximum maturity, so it is capped.
        {
            let nf = vec![NeuronsFundNeuron {
                id: N1,
                maturity_equivalent_icp_e8s: minimum_capped_maturity_icp_e8s,
                controller: *PRINCIPAL_ID_1,
            }];
            let initial_neurons_fund_participation = NeuronsFundParticipation::new(
                SWAP_LIMITS,
                nf.clone(),
                Box::new(SimpleLinearFunction {}),
            )
            .unwrap();
            let final_neurons_fund_participation = initial_neurons_fund_participation
                .from_initial_participation(SWAP_LIMITS.min_direct_participation_icp_e8s)
                .unwrap();
            // I. Validate the public API
            assert!(!initial_neurons_fund_participation.is_empty());
            assert_eq!(initial_neurons_fund_participation.num_neurons(), 1);
            assert_eq!(
                initial_neurons_fund_participation.total_amount_icp_e8s(),
                take_max_initial_neurons_fund_participation_percentage(
                    maximum_uncapped_maturity_icp_e8s
                )
            );
            assert!(!final_neurons_fund_participation.is_empty());
            assert_eq!(final_neurons_fund_participation.num_neurons(), 1);
            assert_eq!(
                final_neurons_fund_participation.total_amount_icp_e8s(),
                take_max_initial_neurons_fund_participation_percentage(
                    maximum_uncapped_maturity_icp_e8s
                )
            );

            // II. Validate the private state
            assert_matches!(
                initial_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert!(minimum_capped_maturity_icp_e8s > maximum_uncapped_maturity_icp_e8s); // Sanity check the test
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N1,
                                amount_icp_e8s: take_max_initial_neurons_fund_participation_percentage(maximum_uncapped_maturity_icp_e8s),
                                maturity_equivalent_icp_e8s: minimum_capped_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_1,
                                is_capped: true, // note the cap
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.max_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, minimum_capped_maturity_icp_e8s);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
            assert_matches!(
                final_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert!(minimum_capped_maturity_icp_e8s > maximum_uncapped_maturity_icp_e8s); // Sanity check the test
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N1,
                                amount_icp_e8s: take_max_initial_neurons_fund_participation_percentage(maximum_uncapped_maturity_icp_e8s),
                                maturity_equivalent_icp_e8s: minimum_capped_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_1,
                                is_capped: true, // note the cap
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, minimum_capped_maturity_icp_e8s);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
        }
    }

    #[test]
    fn test_neurons_fund_participation_with_three_neurons() {
        // Test case A: all neurons can participate.
        {
            let uncapped_eligible_participation =
                (SWAP_LIMITS.min_participant_icp_e8s + SWAP_LIMITS.max_participant_icp_e8s) / 2;
            let uncapped_eligible_maturity = 10 * uncapped_eligible_participation;
            let nf = vec![
                NeuronsFundNeuron {
                    id: N1,
                    maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                    controller: *PRINCIPAL_ID_1,
                },
                NeuronsFundNeuron {
                    id: N2,
                    maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                    controller: *PRINCIPAL_ID_2,
                },
                NeuronsFundNeuron {
                    id: N3,
                    maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                    controller: *PRINCIPAL_ID_3,
                },
            ];
            let initial_neurons_fund_participation = NeuronsFundParticipation::new(
                SWAP_LIMITS,
                nf.clone(),
                Box::new(SimpleLinearFunction {}),
            )
            .unwrap();
            let final_neurons_fund_participation = initial_neurons_fund_participation
                .from_initial_participation(SWAP_LIMITS.min_direct_participation_icp_e8s)
                .unwrap();
            // I. Validate the public API
            assert!(!initial_neurons_fund_participation.is_empty());
            assert_eq!(initial_neurons_fund_participation.num_neurons(), 3);
            assert_eq!(
                initial_neurons_fund_participation.total_amount_icp_e8s(),
                uncapped_eligible_participation * 3
            );
            assert!(!final_neurons_fund_participation.is_empty());
            assert_eq!(final_neurons_fund_participation.num_neurons(), 3);
            // Together, N1, N2, and N3 have more maturity than is needed for Matched Funding.
            // The Neurons' Fund will thus take the proportional participation amounts from these
            // neurons, satisfying the participation amount that corresponds to the final direct
            // participation (SWAP_LIMITS.min_direct_participation_icp_e8s in this case).
            // Since we're using `SimpleLinearFunction` in tests, we get a 1:1 matching.
            assert_eq!(
                final_neurons_fund_participation.total_amount_icp_e8s(),
                SWAP_LIMITS.min_direct_participation_icp_e8s
            );

            // II. Validate the public API
            assert_matches!(
                initial_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N1,
                                amount_icp_e8s: uncapped_eligible_participation,
                                maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                                controller: *PRINCIPAL_ID_1,
                                is_capped: false,
                            },
                            NeuronsFundNeuronPortion {
                                id: N2,
                                amount_icp_e8s: uncapped_eligible_participation,
                                maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                                controller: *PRINCIPAL_ID_2,
                                is_capped: false,
                            },
                            NeuronsFundNeuronPortion {
                                id: N3,
                                amount_icp_e8s: uncapped_eligible_participation,
                                maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                                controller: *PRINCIPAL_ID_3,
                                is_capped: false,
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.max_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, 3 * uncapped_eligible_maturity);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(3 * uncapped_eligible_maturity));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(3 * uncapped_eligible_maturity));
                }
            );
            assert_matches!(
                final_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N1,
                                amount_icp_e8s: SWAP_LIMITS.min_direct_participation_icp_e8s / 3,
                                maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                                controller: *PRINCIPAL_ID_1,
                                is_capped: false,
                            },
                            NeuronsFundNeuronPortion {
                                id: N2,
                                amount_icp_e8s: SWAP_LIMITS.min_direct_participation_icp_e8s / 3,
                                maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                                controller: *PRINCIPAL_ID_2,
                                is_capped: false,
                            },
                            NeuronsFundNeuronPortion {
                                id: N3,
                                amount_icp_e8s: SWAP_LIMITS.min_direct_participation_icp_e8s / 3,
                                maturity_equivalent_icp_e8s: uncapped_eligible_maturity,
                                controller: *PRINCIPAL_ID_3,
                                is_capped: false,
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, 3 * uncapped_eligible_maturity);
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(3 * uncapped_eligible_maturity));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                }
            );
        }
        // Test case B: N1 is ineligible, N2 is capped.
        {
            let maximum_insufficient_maturity_icp_e8s =
                10 * (SWAP_LIMITS.min_participant_icp_e8s - 1);
            let maximal_uncapped_maturity_icp_e8s = 10 * SWAP_LIMITS.max_participant_icp_e8s;
            // Why we pick these particular values for `maturity_equivalent_icp_e8s`:
            // * Total NF maturity: 3 * maximal_uncapped_maturity_icp_e8s.
            // * Initially, N1 is ineligible; N2 is uncapped; N3 is capped.
            // * In the final configuration, N1 is ineligible; N2 and N3 are capped.
            let nf = vec![
                NeuronsFundNeuron {
                    id: N1,
                    maturity_equivalent_icp_e8s: maximum_insufficient_maturity_icp_e8s,
                    controller: *PRINCIPAL_ID_1,
                },
                NeuronsFundNeuron {
                    id: N2,
                    maturity_equivalent_icp_e8s: maximal_uncapped_maturity_icp_e8s,
                    controller: *PRINCIPAL_ID_2,
                },
                NeuronsFundNeuron {
                    id: N3,
                    maturity_equivalent_icp_e8s: 2 * maximal_uncapped_maturity_icp_e8s
                        - maximum_insufficient_maturity_icp_e8s,
                    controller: *PRINCIPAL_ID_3,
                },
            ];
            let initial_neurons_fund_participation = NeuronsFundParticipation::new(
                SWAP_LIMITS,
                nf.clone(),
                Box::new(SimpleLinearFunction {}),
            )
            .unwrap();
            let final_neurons_fund_participation = initial_neurons_fund_participation
                .from_initial_participation(SWAP_LIMITS.min_direct_participation_icp_e8s)
                .unwrap();
            assert!(!initial_neurons_fund_participation.is_empty());
            assert_eq!(initial_neurons_fund_participation.num_neurons(), 2);
            assert_eq!(
                initial_neurons_fund_participation.total_amount_icp_e8s(),
                SWAP_LIMITS.max_participant_icp_e8s * 2
            );
            assert!(!final_neurons_fund_participation.is_empty());
            assert_eq!(final_neurons_fund_participation.num_neurons(), 2);
            // Only N2 and N3 are eligible, N1 is too small. So the final participation maturity is
            // `SWAP_LIMITS.min_direct_participation_icp_e8s * (N1+N2/(N1+N2+N3))`, i.e.
            let weight_n2 = u64_to_dec(maximal_uncapped_maturity_icp_e8s)
                / u64_to_dec(3 * maximal_uncapped_maturity_icp_e8s);
            let weight_n3 = u64_to_dec(
                2 * maximal_uncapped_maturity_icp_e8s - maximum_insufficient_maturity_icp_e8s,
            ) / u64_to_dec(3 * maximal_uncapped_maturity_icp_e8s);

            assert!(
                u64_to_dec(final_neurons_fund_participation.total_amount_icp_e8s())
                    - u64_to_dec(SWAP_LIMITS.min_direct_participation_icp_e8s)
                        * (weight_n2 + weight_n3)
                    < dec!(1.0) // rounding error
            );
            assert_matches!(
                initial_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N2,
                                amount_icp_e8s: SWAP_LIMITS.max_participant_icp_e8s,
                                maturity_equivalent_icp_e8s: maximal_uncapped_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_2,
                                is_capped: false,
                            },
                            NeuronsFundNeuronPortion {
                                id: N3,
                                amount_icp_e8s: SWAP_LIMITS.max_participant_icp_e8s,
                                maturity_equivalent_icp_e8s: 2 * maximal_uncapped_maturity_icp_e8s
                                    - maximum_insufficient_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_3,
                                is_capped: true,
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.max_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, total_nf_maturity_icp_e8s(&nf));
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                }
            );
            assert_matches!(
                final_neurons_fund_participation,
                NeuronsFundParticipation {
                    neurons_fund_reserves,
                    direct_participation_icp_e8s,
                    total_maturity_equivalent_icp_e8s,
                    intended_neurons_fund_participation_icp_e8s,
                    max_neurons_fund_swap_participation_icp_e8s,
                    ..
                } => {
                    assert_eq!(neurons_fund_reserves, NeuronsFundSnapshot::new(
                        vec![
                            NeuronsFundNeuronPortion {
                                id: N2,
                                amount_icp_e8s: dec_to_u64(u64_to_dec(SWAP_LIMITS.min_direct_participation_icp_e8s) * weight_n2).unwrap(),
                                maturity_equivalent_icp_e8s: maximal_uncapped_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_2,
                                is_capped: false,
                            },
                            NeuronsFundNeuronPortion {
                                id: N3,
                                amount_icp_e8s: dec_to_u64(u64_to_dec(SWAP_LIMITS.min_direct_participation_icp_e8s) * weight_n3).unwrap(),
                                maturity_equivalent_icp_e8s: 2 * maximal_uncapped_maturity_icp_e8s
                                    - maximum_insufficient_maturity_icp_e8s,
                                controller: *PRINCIPAL_ID_3,
                                is_capped: false,
                            },
                        ]
                    ));
                    assert_eq!(direct_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                    assert_eq!(total_maturity_equivalent_icp_e8s, total_nf_maturity_icp_e8s(&nf));
                    assert_eq!(max_neurons_fund_swap_participation_icp_e8s, take_max_initial_neurons_fund_participation_percentage(total_nf_maturity_icp_e8s(&nf)));
                    assert_eq!(intended_neurons_fund_participation_icp_e8s, SWAP_LIMITS.min_direct_participation_icp_e8s);
                }
            );
        }
    }
}
