// This file is part of Substrate.

// Copyright (C) 2019-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Transaction Payment Module
//!
//! This module provides the basic logic needed to pay the absolute minimum amount needed for a
//! transaction to be included. This includes:
//!   - _weight fee_: A fee proportional to amount of weight a transaction consumes.
//!   - _length fee_: A fee proportional to the encoded length of the transaction.
//!   - _tip_: An optional tip. Tip increases the priority of the transaction, giving it a higher
//!     chance to be included by the transaction queue.
//!
//! Additionally, this module allows one to configure:
//!   - The mapping between one unit of weight to one unit of fee via [`Trait::WeightToFee`].
//!   - A means of updating the fee for the next block, via defining a multiplier, based on the
//!     final state of the chain at the end of the previous block. This can be configured via
//!     [`Trait::FeeMultiplierUpdate`]

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use frame_support::{
	decl_module, decl_storage,
	dispatch::DispatchResult,
	traits::{Currency, ExistenceRequirement, Get, Imbalance, OnUnbalanced, WithdrawReason},
	weights::{
		DispatchInfo, GetDispatchInfo, Pays, PostDispatchInfo, Weight, WeightToFeeCoefficient,
		WeightToFeePolynomial,
	},
};
use pallet_transaction_payment_rpc_runtime_api::RuntimeDispatchInfo;
use sp_runtime::{
	traits::{
		Convert, DispatchInfoOf, Dispatchable, PostDispatchInfoOf, SaturatedConversion, Saturating,
		SignedExtension, Zero,
	},
	transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionValidity, TransactionValidityError,
		ValidTransaction,
	},
	FixedPointNumber, FixedPointOperand, FixedU128, Perquintill, RuntimeDebug,
};
use sp_std::prelude::*;

/// Fee multiplier.
pub type Multiplier = FixedU128;

type BalanceOf<T> =
	<<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::Balance;
type NegativeImbalanceOf<T> =
	<<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::NegativeImbalance;

/// A struct to update the weight multiplier per block. It implements `Convert<Multiplier,
/// Multiplier>`, meaning that it can convert the previous multiplier to the next one. This should
/// be called on `on_finalize` of a block, prior to potentially cleaning the weight data from the
/// system module.
///
/// given:
/// 	s = previous block weight
/// 	s'= ideal block weight
/// 	m = maximum block weight
///		diff = (s - s')/m
///		v = 0.00001
///		t1 = (v * diff)
///		t2 = (v * diff)^2 / 2
///	then:
/// 	next_multiplier = prev_multiplier * (1 + t1 + t2)
///
/// Where `(s', v)` must be given as the `Get` implementation of the `T` generic type. Moreover, `M`
/// must provide the minimum allowed value for the multiplier. Note that a runtime should ensure
/// with tests that the combination of this `M` and `V` is not such that the multiplier can drop to
/// zero and never recover.
///
/// note that `s'` is interpreted as a portion in the _normal transaction_ capacity of the block.
/// For example, given `s' == 0.25` and `AvailableBlockRatio = 0.75`, then the target fullness is
/// _0.25 of the normal capacity_ and _0.1875 of the entire block_.
///
/// This implementation implies the bound:
/// - `v ≤ p / k * (s − s')`
/// - or, solving for `p`: `p >= v * k * (s - s')`
///
/// where `p` is the amount of change over `k` blocks.
///
/// Hence:
/// - in a fully congested chain: `p >= v * k * (1 - s')`.
/// - in an empty chain: `p >= v * k * (-s')`.
///
/// For example, when all blocks are full and there are 28800 blocks per day (default in
/// `substrate-node`) and v == 0.00001, s' == 0.1875, we'd have:
///
/// p >= 0.00001 * 28800 * 0.8125
/// p >= 0.234
///
/// Meaning that fees can change by around ~23% per day, given extreme congestion.
///
/// More info can be found at:
/// https://w3f-research.readthedocs.io/en/latest/polkadot/Token%20Economics.html
pub struct TargetedFeeAdjustment<T, S, V, M>(sp_std::marker::PhantomData<(T, S, V, M)>);

/// Something that can convert the current multiplier to the next one.
pub trait MultiplierUpdate: Convert<Multiplier, Multiplier> {
	/// Minimum multiplier
	fn min() -> Multiplier;
	/// Target block saturation level
	fn target() -> Perquintill;
	/// Variability factor
	fn variability() -> Multiplier;
}

impl MultiplierUpdate for () {
	fn min() -> Multiplier {
		Default::default()
	}
	fn target() -> Perquintill {
		Default::default()
	}
	fn variability() -> Multiplier {
		Default::default()
	}
}

impl<T, S, V, M> MultiplierUpdate for TargetedFeeAdjustment<T, S, V, M>
where
	T: frame_system::Trait,
	S: Get<Perquintill>,
	V: Get<Multiplier>,
	M: Get<Multiplier>,
{
	fn min() -> Multiplier {
		M::get()
	}
	fn target() -> Perquintill {
		S::get()
	}
	fn variability() -> Multiplier {
		V::get()
	}
}

impl<T, S, V, M> Convert<Multiplier, Multiplier> for TargetedFeeAdjustment<T, S, V, M>
where
	T: frame_system::Trait,
	S: Get<Perquintill>,
	V: Get<Multiplier>,
	M: Get<Multiplier>,
{
	fn convert(previous: Multiplier) -> Multiplier {
		// Defensive only. The multiplier in storage should always be at most positive. Nonetheless
		// we recover here in case of errors, because any value below this would be stale and can
		// never change.
		let min_multiplier = M::get();
		let previous = previous.max(min_multiplier);

		// the computed ratio is only among the normal class.
		let normal_max_weight = <T as frame_system::Trait>::AvailableBlockRatio::get() *
			<T as frame_system::Trait>::MaximumBlockWeight::get();
		let normal_block_weight = <frame_system::Module<T>>::block_weight()
			.get(frame_support::weights::DispatchClass::Normal)
			.min(normal_max_weight);

		let s = S::get();
		let v = V::get();

		let target_weight = (s * normal_max_weight) as u128;
		let block_weight = normal_block_weight as u128;

		// determines if the first_term is positive
		let positive = block_weight >= target_weight;
		let diff_abs = block_weight.max(target_weight) - block_weight.min(target_weight);

		// defensive only, a test case assures that the maximum weight diff can fit in Multiplier
		// without any saturation.
		let diff = Multiplier::saturating_from_rational(diff_abs, normal_max_weight.max(1));
		let diff_squared = diff.saturating_mul(diff);

		let v_squared_2 = v.saturating_mul(v) / Multiplier::saturating_from_integer(2);

		let first_term = v.saturating_mul(diff);
		let second_term = v_squared_2.saturating_mul(diff_squared);

		if positive {
			let excess = first_term.saturating_add(second_term).saturating_mul(previous);
			previous.saturating_add(excess).max(min_multiplier)
		} else {
			// Defensive-only: first_term > second_term. Safe subtraction.
			let negative = first_term.saturating_sub(second_term).saturating_mul(previous);
			previous.saturating_sub(negative).max(min_multiplier)
		}
	}
}

/// Storage releases of the module.
#[derive(Encode, Decode, Clone, Copy, PartialEq, Eq, RuntimeDebug)]
enum Releases {
	/// Original version of the module.
	V1Ancient,
	/// One that bumps the usage to FixedU128 from FixedI128.
	V2,
}

impl Default for Releases {
	fn default() -> Self {
		Releases::V1Ancient
	}
}

pub trait Trait: frame_system::Trait {
	/// The currency type in which fees will be paid.
	type Currency: Currency<Self::AccountId> + Send + Sync;

	/// Handler for the unbalanced reduction when taking transaction fees. This is either one or
	/// two separate imbalances, the first is the transaction fee paid, the second is the tip paid,
	/// if any.
	type OnTransactionPayment: OnUnbalanced<NegativeImbalanceOf<Self>>;

	/// The fee to be paid for making a transaction; the per-byte portion.
	type TransactionByteFee: Get<BalanceOf<Self>>;

	/// Convert a weight value into a deductible fee based on the currency type.
	type WeightToFee: WeightToFeePolynomial<Balance = BalanceOf<Self>>;

	/// Update the multiplier of the next block, based on the previous block's weight.
	type FeeMultiplierUpdate: MultiplierUpdate;
}

decl_storage! {
	trait Store for Module<T: Trait> as TransactionPayment {
		pub NextFeeMultiplier get(fn next_fee_multiplier): Multiplier = Multiplier::saturating_from_integer(1);

		StorageVersion build(|_: &GenesisConfig| Releases::V2): Releases;
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		/// The fee to be paid for making a transaction; the per-byte portion.
		const TransactionByteFee: BalanceOf<T> = T::TransactionByteFee::get();

		/// The polynomial that is applied in order to derive fee from weight.
		const WeightToFee: Vec<WeightToFeeCoefficient<BalanceOf<T>>> =
			T::WeightToFee::polynomial().to_vec();

		fn on_finalize() {
			NextFeeMultiplier::mutate(|fm| {
				*fm = T::FeeMultiplierUpdate::convert(*fm);
			});
		}

		fn integrity_test() {
			// given weight == u64, we build multipliers from `diff` of two weight values, which can
			// at most be MaximumBlockWeight. Make sure that this can fit in a multiplier without
			// loss.
			use sp_std::convert::TryInto;
			assert!(
				<Multiplier as sp_runtime::traits::Bounded>::max_value() >=
				Multiplier::checked_from_integer(
					<T as frame_system::Trait>::MaximumBlockWeight::get().try_into().unwrap()
				).unwrap(),
			);

			// This is the minimum value of the multiplier. Make sure that if we collapse to this
			// value, we can recover with a reasonable amount of traffic. For this test we assert
			// that if we collapse to minimum, the trend will be positive with a weight value
			// which is 1% more than the target.
			let min_value = T::FeeMultiplierUpdate::min();
			let mut target =
				T::FeeMultiplierUpdate::target() *
				(T::AvailableBlockRatio::get() * T::MaximumBlockWeight::get());

			// add 1 percent;
			let addition = target / 100;
			if addition == 0 {
				// this is most likely because in a test setup we set everything to ().
				return;
			}
			target += addition;

			sp_io::TestExternalities::new_empty().execute_with(|| {
				<frame_system::Module<T>>::set_block_limits(target, 0);
				let next = T::FeeMultiplierUpdate::convert(min_value);
				assert!(next > min_value, "The minimum bound of the multiplier is too low. When \
					block saturation is more than target by 1% and multiplier is minimal then \
					the multiplier doesn't increase."
				);
			})
		}
	}
}

impl<T: Trait> Module<T>
where
	BalanceOf<T>: FixedPointOperand,
{
	/// Query the data that we know about the fee of a given `call`.
	///
	/// This module is not and cannot be aware of the internals of a signed extension, for example
	/// a tip. It only interprets the extrinsic as some encoded value and accounts for its weight
	/// and length, the runtime's extrinsic base weight, and the current fee multiplier.
	///
	/// All dispatchables must be annotated with weight and will have some fee info. This function
	/// always returns.
	pub fn query_info<Extrinsic: GetDispatchInfo>(
		unchecked_extrinsic: Extrinsic,
		len: u32,
	) -> RuntimeDispatchInfo<BalanceOf<T>>
	where
		T: Send + Sync,
		BalanceOf<T>: Send + Sync,
		T::Call: Dispatchable<Info = DispatchInfo>,
	{
		// NOTE: we can actually make it understand `ChargeTransactionPayment`, but would be some
		// hassle for sure. We have to make it aware of the index of `ChargeTransactionPayment` in
		// `Extra`. Alternatively, we could actually execute the tx's per-dispatch and record the
		// balance of the sender before and after the pipeline.. but this is way too much hassle for
		// a very very little potential gain in the future.
		let dispatch_info = <Extrinsic as GetDispatchInfo>::get_dispatch_info(&unchecked_extrinsic);

		let partial_fee = Self::compute_fee(len, &dispatch_info, 0u32.into());
		let DispatchInfo { weight, class, .. } = dispatch_info;

		RuntimeDispatchInfo { weight, class, partial_fee }
	}

	/// Compute the final fee value for a particular transaction.
	///
	/// The final fee is composed of:
	///   - `base_fee`: This is the minimum amount a user pays for a transaction. It is declared as
	///     a base _weight_ in the runtime and converted to a fee using `WeightToFee`.
	///   - `len_fee`: The length fee, the amount paid for the encoded length (in bytes) of the
	///     transaction.
	///   - `weight_fee`: This amount is computed based on the weight of the transaction. Weight
	///     accounts for the execution time of a transaction.
	///   - `targeted_fee_adjustment`: This is a multiplier that can tune the final fee based on the
	///     congestion of the network.
	///   - (Optional) `tip`: If included in the transaction, the tip will be added on top. Only
	///     signed transactions can have a tip.
	///
	/// The base fee and adjusted weight and length fees constitute the _inclusion fee,_ which is
	/// the minimum fee for a transaction to be included in a block.
	///
	/// ```ignore
	/// inclusion_fee = base_fee + len_fee + [targeted_fee_adjustment * weight_fee];
	/// final_fee = inclusion_fee + tip;
	/// ```
	pub fn compute_fee(len: u32, info: &DispatchInfoOf<T::Call>, tip: BalanceOf<T>) -> BalanceOf<T>
	where
		T::Call: Dispatchable<Info = DispatchInfo>,
	{
		Self::compute_fee_raw(len, info.weight, tip, info.pays_fee)
	}

	/// Compute the actual post dispatch fee for a particular transaction.
	///
	/// Identical to `compute_fee` with the only difference that the post dispatch corrected
	/// weight is used for the weight fee calculation.
	pub fn compute_actual_fee(
		len: u32,
		info: &DispatchInfoOf<T::Call>,
		post_info: &PostDispatchInfoOf<T::Call>,
		tip: BalanceOf<T>,
	) -> BalanceOf<T>
	where
		T::Call: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	{
		Self::compute_fee_raw(
			len,
			post_info.calc_actual_weight(info),
			tip,
			post_info.pays_fee(info),
		)
	}

	fn compute_fee_raw(
		len: u32,
		weight: Weight,
		tip: BalanceOf<T>,
		pays_fee: Pays,
	) -> BalanceOf<T> {
		if pays_fee == Pays::Yes {
			let len = <BalanceOf<T>>::from(len);
			let per_byte = T::TransactionByteFee::get();

			// length fee. this is not adjusted.
			let fixed_len_fee = per_byte.saturating_mul(len);

			// the adjustable part of the fee.
			let unadjusted_weight_fee = Self::weight_to_fee(weight);
			let multiplier = Self::next_fee_multiplier();
			// final adjusted weight fee.
			let adjusted_weight_fee = multiplier.saturating_mul_int(unadjusted_weight_fee);

			let base_fee = Self::weight_to_fee(T::ExtrinsicBaseWeight::get());
			base_fee
				.saturating_add(fixed_len_fee)
				.saturating_add(adjusted_weight_fee)
				.saturating_add(tip)
		} else {
			tip
		}
	}

	fn weight_to_fee(weight: Weight) -> BalanceOf<T> {
		// cap the weight to the maximum defined in runtime, otherwise it will be the
		// `Bounded` maximum of its data type, which is not desired.
		let capped_weight = weight.min(<T as frame_system::Trait>::MaximumBlockWeight::get());
		T::WeightToFee::calc(&capped_weight)
	}
}

impl<T> Convert<Weight, BalanceOf<T>> for Module<T>
where
	T: Trait,
	BalanceOf<T>: FixedPointOperand,
{
	/// Compute the fee for the specified weight.
	///
	/// This fee is already adjusted by the per block fee adjustment factor and is therefore the
	/// share that the weight contributes to the overall fee of a transaction. It is mainly
	/// for informational purposes and not used in the actual fee calculation.
	fn convert(weight: Weight) -> BalanceOf<T> {
		NextFeeMultiplier::get().saturating_mul_int(Self::weight_to_fee(weight))
	}
}

/// Require the transactor pay for themselves and maybe include a tip to gain additional priority
/// in the queue.
#[derive(Encode, Decode, Clone, Eq, PartialEq)]
pub struct ChargeTransactionPayment<T: Trait + Send + Sync>(#[codec(compact)] BalanceOf<T>);

impl<T: Trait + Send + Sync> ChargeTransactionPayment<T>
where
	T::Call: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	BalanceOf<T>: Send + Sync + FixedPointOperand,
{
	/// utility constructor. Used only in client/factory code.
	pub fn from(fee: BalanceOf<T>) -> Self {
		Self(fee)
	}

	fn withdraw_fee(
		&self,
		who: &T::AccountId,
		info: &DispatchInfoOf<T::Call>,
		len: usize,
	) -> Result<(BalanceOf<T>, Option<NegativeImbalanceOf<T>>), TransactionValidityError> {
		let tip = self.0;
		let fee = Module::<T>::compute_fee(len as u32, info, tip);

		// Only mess with balances if fee is not zero.
		if fee.is_zero() {
			return Ok((fee, None))
		}

		match T::Currency::withdraw(
			who,
			fee,
			if tip.is_zero() {
				WithdrawReason::TransactionPayment.into()
			} else {
				WithdrawReason::TransactionPayment | WithdrawReason::Tip
			},
			ExistenceRequirement::KeepAlive,
		) {
			Ok(imbalance) => Ok((fee, Some(imbalance))),
			Err(_) => Err(InvalidTransaction::Payment.into()),
		}
	}

	/// Get an appropriate priority for a transaction with the given length and info.
	///
	/// This will try and optimise the `fee/weight` `fee/length`, whichever is consuming more of the
	/// maximum corresponding limit.
	///
	/// For example, if a transaction consumed 1/4th of the block length and half of the weight, its
	/// final priority is `fee * min(2, 4) = fee * 2`. If it consumed `1/4th` of the block length
	/// and the entire block weight `(1/1)`, its priority is `fee * min(1, 4) = fee * 1`. This means
	///  that the transaction which consumes more resources (either length or weight) with the same
	/// `fee` ends up having lower priority.
	fn get_priority(
		len: usize,
		info: &DispatchInfoOf<T::Call>,
		final_fee: BalanceOf<T>,
	) -> TransactionPriority {
		let weight_saturation = T::MaximumBlockWeight::get() / info.weight.max(1);
		let len_saturation = T::MaximumBlockLength::get() as u64 / (len as u64).max(1);
		let coefficient: BalanceOf<T> =
			weight_saturation.min(len_saturation).saturated_into::<BalanceOf<T>>();
		final_fee.saturating_mul(coefficient).saturated_into::<TransactionPriority>()
	}
}

impl<T: Trait + Send + Sync> sp_std::fmt::Debug for ChargeTransactionPayment<T> {
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		write!(f, "ChargeTransactionPayment<{:?}>", self.0)
	}
	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		Ok(())
	}
}

impl<T: Trait + Send + Sync> SignedExtension for ChargeTransactionPayment<T>
where
	BalanceOf<T>: Send + Sync + From<u64> + FixedPointOperand,
	T::Call: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
{
	const IDENTIFIER: &'static str = "ChargeTransactionPayment";
	type AccountId = T::AccountId;
	type Call = T::Call;
	type AdditionalSigned = ();
	type Pre = (BalanceOf<T>, Self::AccountId, Option<NegativeImbalanceOf<T>>, BalanceOf<T>);
	fn additional_signed(&self) -> sp_std::result::Result<(), TransactionValidityError> {
		Ok(())
	}

	fn validate(
		&self,
		who: &Self::AccountId,
		_call: &Self::Call,
		info: &DispatchInfoOf<Self::Call>,
		len: usize,
	) -> TransactionValidity {
		let (fee, _) = self.withdraw_fee(who, info, len)?;
		Ok(ValidTransaction { priority: Self::get_priority(len, info, fee), ..Default::default() })
	}

	fn pre_dispatch(
		self,
		who: &Self::AccountId,
		_call: &Self::Call,
		info: &DispatchInfoOf<Self::Call>,
		len: usize,
	) -> Result<Self::Pre, TransactionValidityError> {
		let (fee, imbalance) = self.withdraw_fee(who, info, len)?;
		Ok((self.0, who.clone(), imbalance, fee))
	}

	fn post_dispatch(
		pre: Self::Pre,
		info: &DispatchInfoOf<Self::Call>,
		post_info: &PostDispatchInfoOf<Self::Call>,
		len: usize,
		_result: &DispatchResult,
	) -> Result<(), TransactionValidityError> {
		let (tip, who, imbalance, fee) = pre;
		if let Some(payed) = imbalance {
			let actual_fee = Module::<T>::compute_actual_fee(len as u32, info, post_info, tip);
			let refund = fee.saturating_sub(actual_fee);
			let actual_payment = match T::Currency::deposit_into_existing(&who, refund) {
				Ok(refund_imbalance) => {
					// The refund cannot be larger than the up front payed max weight.
					// `PostDispatchInfo::calc_unspent` guards against such a case.
					match payed.offset(refund_imbalance) {
						Ok(actual_payment) => actual_payment,
						Err(_) => return Err(InvalidTransaction::Payment.into()),
					}
				},
				// We do not recreate the account using the refund. The up front payment
				// is gone in that case.
				Err(_) => payed,
			};
			let imbalances = actual_payment.split(tip);
			T::OnTransactionPayment::on_unbalanceds(
				Some(imbalances.0).into_iter().chain(Some(imbalances.1)),
			);
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use codec::Encode;
	use frame_support::{
		impl_outer_dispatch, impl_outer_event, impl_outer_origin, parameter_types,
		weights::{
			DispatchClass, DispatchInfo, GetDispatchInfo, PostDispatchInfo, Weight,
			WeightToFeeCoefficient, WeightToFeeCoefficients, WeightToFeePolynomial,
		},
	};
	use pallet_balances::Call as BalancesCall;
	use pallet_transaction_payment_rpc_runtime_api::RuntimeDispatchInfo;
	use smallvec::smallvec;
	use sp_core::H256;
	use sp_runtime::{
		testing::{Header, TestXt},
		traits::{BlakeTwo256, IdentityLookup},
		Perbill,
	};
	use std::cell::RefCell;

	const CALL: &<Runtime as frame_system::Trait>::Call =
		&Call::Balances(BalancesCall::transfer(2, 69));

	impl_outer_dispatch! {
		pub enum Call for Runtime where origin: Origin {
			pallet_balances::Balances,
			frame_system::System,
		}
	}

	impl_outer_event! {
		pub enum Event for Runtime {
			system<T>,
			pallet_balances<T>,
		}
	}

	#[derive(Clone, PartialEq, Eq, Debug)]
	pub struct Runtime;

	use frame_system as system;
	impl_outer_origin! {
		pub enum Origin for Runtime {}
	}

	thread_local! {
		static EXTRINSIC_BASE_WEIGHT: RefCell<u64> = RefCell::new(0);
	}

	pub struct ExtrinsicBaseWeight;
	impl Get<u64> for ExtrinsicBaseWeight {
		fn get() -> u64 {
			EXTRINSIC_BASE_WEIGHT.with(|v| *v.borrow())
		}
	}

	parameter_types! {
		pub const BlockHashCount: u64 = 250;
		pub const MaximumBlockWeight: Weight = 1024;
		pub const MaximumBlockLength: u32 = 2 * 1024;
		pub const AvailableBlockRatio: Perbill = Perbill::one();
	}

	impl frame_system::Trait for Runtime {
		type BaseCallFilter = ();
		type Origin = Origin;
		type Index = u64;
		type BlockNumber = u64;
		type Call = Call;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = Event;
		type BlockHashCount = BlockHashCount;
		type MaximumBlockWeight = MaximumBlockWeight;
		type DbWeight = ();
		type BlockExecutionWeight = ();
		type ExtrinsicBaseWeight = ExtrinsicBaseWeight;
		type MaximumExtrinsicWeight = MaximumBlockWeight;
		type MaximumBlockLength = MaximumBlockLength;
		type AvailableBlockRatio = AvailableBlockRatio;
		type Version = ();
		type PalletInfo = ();
		type AccountData = pallet_balances::AccountData<u64>;
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
	}

	parameter_types! {
		pub const ExistentialDeposit: u64 = 1;
	}

	impl pallet_balances::Trait for Runtime {
		type Balance = u64;
		type Event = Event;
		type DustRemoval = ();
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = System;
		type MaxLocks = ();
		type WeightInfo = ();
	}
	thread_local! {
		static TRANSACTION_BYTE_FEE: RefCell<u64> = RefCell::new(1);
		static WEIGHT_TO_FEE: RefCell<u64> = RefCell::new(1);
	}

	pub struct TransactionByteFee;
	impl Get<u64> for TransactionByteFee {
		fn get() -> u64 {
			TRANSACTION_BYTE_FEE.with(|v| *v.borrow())
		}
	}

	pub struct WeightToFee;
	impl WeightToFeePolynomial for WeightToFee {
		type Balance = u64;

		fn polynomial() -> WeightToFeeCoefficients<Self::Balance> {
			smallvec![WeightToFeeCoefficient {
				degree: 1,
				coeff_frac: Perbill::zero(),
				coeff_integer: WEIGHT_TO_FEE.with(|v| *v.borrow()),
				negative: false,
			}]
		}
	}

	impl Trait for Runtime {
		type Currency = pallet_balances::Module<Runtime>;
		type OnTransactionPayment = ();
		type TransactionByteFee = TransactionByteFee;
		type WeightToFee = WeightToFee;
		type FeeMultiplierUpdate = ();
	}

	type Balances = pallet_balances::Module<Runtime>;
	type System = frame_system::Module<Runtime>;
	type TransactionPayment = Module<Runtime>;

	pub struct ExtBuilder {
		balance_factor: u64,
		base_weight: u64,
		byte_fee: u64,
		weight_to_fee: u64,
	}

	impl Default for ExtBuilder {
		fn default() -> Self {
			Self { balance_factor: 1, base_weight: 0, byte_fee: 1, weight_to_fee: 1 }
		}
	}

	impl ExtBuilder {
		pub fn base_weight(mut self, base_weight: u64) -> Self {
			self.base_weight = base_weight;
			self
		}
		pub fn byte_fee(mut self, byte_fee: u64) -> Self {
			self.byte_fee = byte_fee;
			self
		}
		pub fn weight_fee(mut self, weight_to_fee: u64) -> Self {
			self.weight_to_fee = weight_to_fee;
			self
		}
		pub fn balance_factor(mut self, factor: u64) -> Self {
			self.balance_factor = factor;
			self
		}
		fn set_constants(&self) {
			EXTRINSIC_BASE_WEIGHT.with(|v| *v.borrow_mut() = self.base_weight);
			TRANSACTION_BYTE_FEE.with(|v| *v.borrow_mut() = self.byte_fee);
			WEIGHT_TO_FEE.with(|v| *v.borrow_mut() = self.weight_to_fee);
		}
		pub fn build(self) -> sp_io::TestExternalities {
			self.set_constants();
			let mut t = frame_system::GenesisConfig::default().build_storage::<Runtime>().unwrap();
			pallet_balances::GenesisConfig::<Runtime> {
				balances: if self.balance_factor > 0 {
					vec![
						(1, 10 * self.balance_factor),
						(2, 20 * self.balance_factor),
						(3, 30 * self.balance_factor),
						(4, 40 * self.balance_factor),
						(5, 50 * self.balance_factor),
						(6, 60 * self.balance_factor),
					]
				} else {
					vec![]
				},
			}
			.assimilate_storage(&mut t)
			.unwrap();
			t.into()
		}
	}

	/// create a transaction info struct from weight. Handy to avoid building the whole struct.
	pub fn info_from_weight(w: Weight) -> DispatchInfo {
		// pays_fee: Pays::Yes -- class: DispatchClass::Normal
		DispatchInfo { weight: w, ..Default::default() }
	}

	fn post_info_from_weight(w: Weight) -> PostDispatchInfo {
		PostDispatchInfo { actual_weight: Some(w), pays_fee: Default::default() }
	}

	fn post_info_from_pays(p: Pays) -> PostDispatchInfo {
		PostDispatchInfo { actual_weight: None, pays_fee: p }
	}

	fn default_post_info() -> PostDispatchInfo {
		PostDispatchInfo { actual_weight: None, pays_fee: Default::default() }
	}

	#[test]
	fn signed_extension_transaction_payment_work() {
		ExtBuilder::default().balance_factor(10).base_weight(5).build().execute_with(|| {
			let len = 10;
			let pre = ChargeTransactionPayment::<Runtime>::from(0)
				.pre_dispatch(&1, CALL, &info_from_weight(5), len)
				.unwrap();
			assert_eq!(Balances::free_balance(1), 100 - 5 - 5 - 10);

			assert!(ChargeTransactionPayment::<Runtime>::post_dispatch(
				pre,
				&info_from_weight(5),
				&default_post_info(),
				len,
				&Ok(())
			)
			.is_ok());
			assert_eq!(Balances::free_balance(1), 100 - 5 - 5 - 10);

			let pre = ChargeTransactionPayment::<Runtime>::from(5 /* tipped */)
				.pre_dispatch(&2, CALL, &info_from_weight(100), len)
				.unwrap();
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 100 - 5);

			assert!(ChargeTransactionPayment::<Runtime>::post_dispatch(
				pre,
				&info_from_weight(100),
				&post_info_from_weight(50),
				len,
				&Ok(())
			)
			.is_ok());
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 50 - 5);
		});
	}

	#[test]
	fn signed_extension_transaction_payment_multiplied_refund_works() {
		ExtBuilder::default().balance_factor(10).base_weight(5).build().execute_with(|| {
			let len = 10;
			NextFeeMultiplier::put(Multiplier::saturating_from_rational(3, 2));

			let pre = ChargeTransactionPayment::<Runtime>::from(5 /* tipped */)
				.pre_dispatch(&2, CALL, &info_from_weight(100), len)
				.unwrap();
			// 5 base fee, 10 byte fee, 3/2 * 100 weight fee, 5 tip
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 150 - 5);

			assert!(ChargeTransactionPayment::<Runtime>::post_dispatch(
				pre,
				&info_from_weight(100),
				&post_info_from_weight(50),
				len,
				&Ok(())
			)
			.is_ok());
			// 75 (3/2 of the returned 50 units of weight) is refunded
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 75 - 5);
		});
	}

	#[test]
	fn signed_extension_transaction_payment_is_bounded() {
		ExtBuilder::default().balance_factor(1000).byte_fee(0).build().execute_with(|| {
			// maximum weight possible
			assert!(ChargeTransactionPayment::<Runtime>::from(0)
				.pre_dispatch(&1, CALL, &info_from_weight(Weight::max_value()), 10)
				.is_ok());
			// fee will be proportional to what is the actual maximum weight in the runtime.
			assert_eq!(
				Balances::free_balance(&1),
				(10000 - <Runtime as frame_system::Trait>::MaximumBlockWeight::get()) as u64
			);
		});
	}

	#[test]
	fn signed_extension_allows_free_transactions() {
		ExtBuilder::default().base_weight(100).balance_factor(0).build().execute_with(|| {
			// 1 ain't have a penny.
			assert_eq!(Balances::free_balance(1), 0);

			let len = 100;

			// This is a completely free (and thus wholly insecure/DoS-ridden) transaction.
			let operational_transaction =
				DispatchInfo { weight: 0, class: DispatchClass::Operational, pays_fee: Pays::No };
			assert!(ChargeTransactionPayment::<Runtime>::from(0)
				.validate(&1, CALL, &operational_transaction, len)
				.is_ok());

			// like a InsecureFreeNormal
			let free_transaction =
				DispatchInfo { weight: 0, class: DispatchClass::Normal, pays_fee: Pays::Yes };
			assert!(ChargeTransactionPayment::<Runtime>::from(0)
				.validate(&1, CALL, &free_transaction, len)
				.is_err());
		});
	}

	#[test]
	fn signed_ext_length_fee_is_also_updated_per_congestion() {
		ExtBuilder::default().base_weight(5).balance_factor(10).build().execute_with(|| {
			// all fees should be x1.5
			NextFeeMultiplier::put(Multiplier::saturating_from_rational(3, 2));
			let len = 10;

			assert!(ChargeTransactionPayment::<Runtime>::from(10) // tipped
				.pre_dispatch(&1, CALL, &info_from_weight(3), len)
				.is_ok());
			assert_eq!(
				Balances::free_balance(1),
				100 // original
				- 10 // tip
				- 5 // base
				- 10 // len
				- (3 * 3 / 2) // adjusted weight
			);
		})
	}

	#[test]
	fn query_info_works() {
		let call = Call::Balances(BalancesCall::transfer(2, 69));
		let origin = 111111;
		let extra = ();
		let xt = TestXt::new(call, Some((origin, extra)));
		let info = xt.get_dispatch_info();
		let ext = xt.encode();
		let len = ext.len() as u32;
		ExtBuilder::default().base_weight(5).weight_fee(2).build().execute_with(|| {
			// all fees should be x1.5
			NextFeeMultiplier::put(Multiplier::saturating_from_rational(3, 2));

			assert_eq!(
				TransactionPayment::query_info(xt, len),
				RuntimeDispatchInfo {
					weight: info.weight,
					class: info.class,
					partial_fee: 5 * 2 /* base * weight_fee */
						+ len as u64  /* len * 1 */
						+ info.weight.min(MaximumBlockWeight::get()) as u64 * 2 * 3 / 2 /* weight */
				},
			);
		});
	}

	#[test]
	fn compute_fee_works_without_multiplier() {
		ExtBuilder::default().base_weight(100).byte_fee(10).balance_factor(0).build().execute_with(
			|| {
				// Next fee multiplier is zero
				assert_eq!(NextFeeMultiplier::get(), Multiplier::one());

				// Tip only, no fees works
				let dispatch_info = DispatchInfo {
					weight: 0,
					class: DispatchClass::Operational,
					pays_fee: Pays::No,
				};
				assert_eq!(Module::<Runtime>::compute_fee(0, &dispatch_info, 10), 10);
				// No tip, only base fee works
				let dispatch_info = DispatchInfo {
					weight: 0,
					class: DispatchClass::Operational,
					pays_fee: Pays::Yes,
				};
				assert_eq!(Module::<Runtime>::compute_fee(0, &dispatch_info, 0), 100);
				// Tip + base fee works
				assert_eq!(Module::<Runtime>::compute_fee(0, &dispatch_info, 69), 169);
				// Len (byte fee) + base fee works
				assert_eq!(Module::<Runtime>::compute_fee(42, &dispatch_info, 0), 520);
				// Weight fee + base fee works
				let dispatch_info = DispatchInfo {
					weight: 1000,
					class: DispatchClass::Operational,
					pays_fee: Pays::Yes,
				};
				assert_eq!(Module::<Runtime>::compute_fee(0, &dispatch_info, 0), 1100);
			},
		);
	}

	#[test]
	fn compute_fee_works_with_multiplier() {
		ExtBuilder::default().base_weight(100).byte_fee(10).balance_factor(0).build().execute_with(
			|| {
				// Add a next fee multiplier. Fees will be x3/2.
				NextFeeMultiplier::put(Multiplier::saturating_from_rational(3, 2));
				// Base fee is unaffected by multiplier
				let dispatch_info = DispatchInfo {
					weight: 0,
					class: DispatchClass::Operational,
					pays_fee: Pays::Yes,
				};
				assert_eq!(Module::<Runtime>::compute_fee(0, &dispatch_info, 0), 100);

				// Everything works together :)
				let dispatch_info = DispatchInfo {
					weight: 123,
					class: DispatchClass::Operational,
					pays_fee: Pays::Yes,
				};
				// 123 weight, 456 length, 100 base
				assert_eq!(
					Module::<Runtime>::compute_fee(456, &dispatch_info, 789),
					100 + (3 * 123 / 2) + 4560 + 789,
				);
			},
		);
	}

	#[test]
	fn compute_fee_works_with_negative_multiplier() {
		ExtBuilder::default().base_weight(100).byte_fee(10).balance_factor(0).build().execute_with(
			|| {
				// Add a next fee multiplier. All fees will be x1/2.
				NextFeeMultiplier::put(Multiplier::saturating_from_rational(1, 2));

				// Base fee is unaffected by multiplier.
				let dispatch_info = DispatchInfo {
					weight: 0,
					class: DispatchClass::Operational,
					pays_fee: Pays::Yes,
				};
				assert_eq!(Module::<Runtime>::compute_fee(0, &dispatch_info, 0), 100);

				// Everything works together.
				let dispatch_info = DispatchInfo {
					weight: 123,
					class: DispatchClass::Operational,
					pays_fee: Pays::Yes,
				};
				// 123 weight, 456 length, 100 base
				assert_eq!(
					Module::<Runtime>::compute_fee(456, &dispatch_info, 789),
					100 + (123 / 2) + 4560 + 789,
				);
			},
		);
	}

	#[test]
	fn compute_fee_does_not_overflow() {
		ExtBuilder::default().base_weight(100).byte_fee(10).balance_factor(0).build().execute_with(
			|| {
				// Overflow is handled
				let dispatch_info = DispatchInfo {
					weight: Weight::max_value(),
					class: DispatchClass::Operational,
					pays_fee: Pays::Yes,
				};
				assert_eq!(
					Module::<Runtime>::compute_fee(
						<u32>::max_value(),
						&dispatch_info,
						<u64>::max_value()
					),
					<u64>::max_value()
				);
			},
		);
	}

	#[test]
	fn refund_does_not_recreate_account() {
		ExtBuilder::default().balance_factor(10).base_weight(5).build().execute_with(|| {
			// So events are emitted
			System::set_block_number(10);
			let len = 10;
			let pre = ChargeTransactionPayment::<Runtime>::from(5 /* tipped */)
				.pre_dispatch(&2, CALL, &info_from_weight(100), len)
				.unwrap();
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 100 - 5);

			// kill the account between pre and post dispatch
			assert!(Balances::transfer(Some(2).into(), 3, Balances::free_balance(2)).is_ok());
			assert_eq!(Balances::free_balance(2), 0);

			assert!(ChargeTransactionPayment::<Runtime>::post_dispatch(
				pre,
				&info_from_weight(100),
				&post_info_from_weight(50),
				len,
				&Ok(())
			)
			.is_ok());
			assert_eq!(Balances::free_balance(2), 0);
			// Transfer Event
			assert!(System::events().iter().any(|event| {
				event.event == Event::pallet_balances(pallet_balances::RawEvent::Transfer(2, 3, 80))
			}));
			// Killed Event
			assert!(System::events()
				.iter()
				.any(|event| { event.event == Event::system(system::RawEvent::KilledAccount(2)) }));
		});
	}

	#[test]
	fn actual_weight_higher_than_max_refunds_nothing() {
		ExtBuilder::default().balance_factor(10).base_weight(5).build().execute_with(|| {
			let len = 10;
			let pre = ChargeTransactionPayment::<Runtime>::from(5 /* tipped */)
				.pre_dispatch(&2, CALL, &info_from_weight(100), len)
				.unwrap();
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 100 - 5);

			assert!(ChargeTransactionPayment::<Runtime>::post_dispatch(
				pre,
				&info_from_weight(100),
				&post_info_from_weight(101),
				len,
				&Ok(())
			)
			.is_ok());
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 100 - 5);
		});
	}

	#[test]
	fn zero_transfer_on_free_transaction() {
		ExtBuilder::default().balance_factor(10).base_weight(5).build().execute_with(|| {
			// So events are emitted
			System::set_block_number(10);
			let len = 10;
			let dispatch_info =
				DispatchInfo { weight: 100, pays_fee: Pays::No, class: DispatchClass::Normal };
			let user = 69;
			let pre = ChargeTransactionPayment::<Runtime>::from(0)
				.pre_dispatch(&user, CALL, &dispatch_info, len)
				.unwrap();
			assert_eq!(Balances::total_balance(&user), 0);
			assert!(ChargeTransactionPayment::<Runtime>::post_dispatch(
				pre,
				&dispatch_info,
				&default_post_info(),
				len,
				&Ok(())
			)
			.is_ok());
			assert_eq!(Balances::total_balance(&user), 0);
			// No events for such a scenario
			assert_eq!(System::events().len(), 0);
		});
	}

	#[test]
	fn refund_consistent_with_actual_weight() {
		ExtBuilder::default().balance_factor(10).base_weight(7).build().execute_with(|| {
			let info = info_from_weight(100);
			let post_info = post_info_from_weight(33);
			let prev_balance = Balances::free_balance(2);
			let len = 10;
			let tip = 5;

			NextFeeMultiplier::put(Multiplier::saturating_from_rational(5, 4));

			let pre = ChargeTransactionPayment::<Runtime>::from(tip)
				.pre_dispatch(&2, CALL, &info, len)
				.unwrap();

			ChargeTransactionPayment::<Runtime>::post_dispatch(
				pre,
				&info,
				&post_info,
				len,
				&Ok(()),
			)
			.unwrap();

			let refund_based_fee = prev_balance - Balances::free_balance(2);
			let actual_fee =
				Module::<Runtime>::compute_actual_fee(len as u32, &info, &post_info, tip);

			// 33 weight, 10 length, 7 base, 5 tip
			assert_eq!(actual_fee, 7 + 10 + (33 * 5 / 4) + 5);
			assert_eq!(refund_based_fee, actual_fee);
		});
	}

	#[test]
	fn post_info_can_change_pays_fee() {
		ExtBuilder::default().balance_factor(10).base_weight(7).build().execute_with(|| {
			let info = info_from_weight(100);
			let post_info = post_info_from_pays(Pays::No);
			let prev_balance = Balances::free_balance(2);
			let len = 10;
			let tip = 5;

			NextFeeMultiplier::put(Multiplier::saturating_from_rational(5, 4));

			let pre = ChargeTransactionPayment::<Runtime>::from(tip)
				.pre_dispatch(&2, CALL, &info, len)
				.unwrap();

			ChargeTransactionPayment::<Runtime>::post_dispatch(
				pre,
				&info,
				&post_info,
				len,
				&Ok(()),
			)
			.unwrap();

			let refund_based_fee = prev_balance - Balances::free_balance(2);
			let actual_fee =
				Module::<Runtime>::compute_actual_fee(len as u32, &info, &post_info, tip);

			// Only 5 tip is paid
			assert_eq!(actual_fee, 5);
			assert_eq!(refund_based_fee, actual_fee);
		});
	}
}
