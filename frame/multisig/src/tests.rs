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

// Tests for Multisig Pallet

#![cfg(test)]

use super::*;

use crate as multisig;
use frame_support::{
	assert_noop, assert_ok, impl_outer_dispatch, impl_outer_event, impl_outer_origin,
	parameter_types, traits::Filter, weights::Weight,
};
use sp_core::H256;
use sp_runtime::{
	testing::Header,
	traits::{BlakeTwo256, IdentityLookup},
	Perbill,
};

impl_outer_origin! {
	pub enum Origin for Test where system = frame_system {}
}

impl_outer_event! {
	pub enum TestEvent for Test {
		system<T>,
		pallet_balances<T>,
		multisig<T>,
	}
}
impl_outer_dispatch! {
	pub enum Call for Test where origin: Origin {
		frame_system::System,
		pallet_balances::Balances,
		multisig::Multisig,
	}
}

// For testing the pallet, we construct most of a mock runtime. This means
// first constructing a configuration type (`Test`) which `impl`s each of the
// configuration traits of pallets we want to use.
#[derive(Clone, Eq, PartialEq)]
pub struct Test;
parameter_types! {
	pub const BlockHashCount: u64 = 250;
	pub const MaximumBlockWeight: Weight = 1024;
	pub const MaximumBlockLength: u32 = 2 * 1024;
	pub const AvailableBlockRatio: Perbill = Perbill::one();
}
impl frame_system::Trait for Test {
	type BaseCallFilter = TestBaseCallFilter;
	type Origin = Origin;
	type Index = u64;
	type BlockNumber = u64;
	type Hash = H256;
	type Call = Call;
	type Hashing = BlakeTwo256;
	type AccountId = u64;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Header = Header;
	type Event = TestEvent;
	type BlockHashCount = BlockHashCount;
	type MaximumBlockWeight = MaximumBlockWeight;
	type DbWeight = ();
	type BlockExecutionWeight = ();
	type ExtrinsicBaseWeight = ();
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
impl pallet_balances::Trait for Test {
	type MaxLocks = ();
	type Balance = u64;
	type Event = TestEvent;
	type DustRemoval = ();
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type WeightInfo = ();
}
parameter_types! {
	pub const DepositBase: u64 = 1;
	pub const DepositFactor: u64 = 1;
	pub const MaxSignatories: u16 = 3;
}
pub struct TestBaseCallFilter;
impl Filter<Call> for TestBaseCallFilter {
	fn filter(c: &Call) -> bool {
		match *c {
			Call::Balances(_) => true,
			// Needed for benchmarking
			Call::System(frame_system::Call::remark(_)) => true,
			_ => false,
		}
	}
}
impl Trait for Test {
	type Event = TestEvent;
	type Call = Call;
	type Currency = Balances;
	type DepositBase = DepositBase;
	type DepositFactor = DepositFactor;
	type MaxSignatories = MaxSignatories;
	type WeightInfo = ();
}
type System = frame_system::Module<Test>;
type Balances = pallet_balances::Module<Test>;
type Multisig = Module<Test>;

use pallet_balances::Call as BalancesCall;
use pallet_balances::Error as BalancesError;

pub fn new_test_ext() -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
	pallet_balances::GenesisConfig::<Test> {
		balances: vec![(1, 10), (2, 10), (3, 10), (4, 10), (5, 2)],
	}
	.assimilate_storage(&mut t)
	.unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

fn last_event() -> TestEvent {
	system::Module::<Test>::events().pop().map(|e| e.event).expect("Event expected")
}

fn expect_event<E: Into<TestEvent>>(e: E) {
	assert_eq!(last_event(), e.into());
}

fn now() -> Timepoint<u64> {
	Multisig::timepoint()
}

#[test]
fn multisig_deposit_is_taken_and_returned() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		assert_ok!(Multisig::as_multi(
			Origin::signed(1),
			2,
			vec![2, 3],
			None,
			data.clone(),
			false,
			0
		));
		assert_eq!(Balances::free_balance(1), 2);
		assert_eq!(Balances::reserved_balance(1), 3);

		assert_ok!(Multisig::as_multi(
			Origin::signed(2),
			2,
			vec![1, 3],
			Some(now()),
			data,
			false,
			call_weight
		));
		assert_eq!(Balances::free_balance(1), 5);
		assert_eq!(Balances::reserved_balance(1), 0);
	});
}

#[test]
fn multisig_deposit_is_taken_and_returned_with_call_storage() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		let hash = blake2_256(&data);
		assert_ok!(Multisig::as_multi(Origin::signed(1), 2, vec![2, 3], None, data, true, 0));
		assert_eq!(Balances::free_balance(1), 0);
		assert_eq!(Balances::reserved_balance(1), 5);

		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(2),
			2,
			vec![1, 3],
			Some(now()),
			hash,
			call_weight
		));
		assert_eq!(Balances::free_balance(1), 5);
		assert_eq!(Balances::reserved_balance(1), 0);
	});
}

#[test]
fn multisig_deposit_is_taken_and_returned_with_alt_call_storage() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 3);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		let hash = blake2_256(&data);

		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			None,
			hash.clone(),
			0
		));
		assert_eq!(Balances::free_balance(1), 1);
		assert_eq!(Balances::reserved_balance(1), 4);

		assert_ok!(Multisig::as_multi(
			Origin::signed(2),
			3,
			vec![1, 3],
			Some(now()),
			data,
			true,
			0
		));
		assert_eq!(Balances::free_balance(2), 3);
		assert_eq!(Balances::reserved_balance(2), 2);
		assert_eq!(Balances::free_balance(1), 1);
		assert_eq!(Balances::reserved_balance(1), 4);

		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(3),
			3,
			vec![1, 2],
			Some(now()),
			hash,
			call_weight
		));
		assert_eq!(Balances::free_balance(1), 5);
		assert_eq!(Balances::reserved_balance(1), 0);
		assert_eq!(Balances::free_balance(2), 5);
		assert_eq!(Balances::reserved_balance(2), 0);
	});
}

#[test]
fn cancel_multisig_returns_deposit() {
	new_test_ext().execute_with(|| {
		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		let hash = blake2_256(&call);
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			None,
			hash.clone(),
			0
		));
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(2),
			3,
			vec![1, 3],
			Some(now()),
			hash.clone(),
			0
		));
		assert_eq!(Balances::free_balance(1), 6);
		assert_eq!(Balances::reserved_balance(1), 4);
		assert_ok!(Multisig::cancel_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			now(),
			hash.clone()
		),);
		assert_eq!(Balances::free_balance(1), 10);
		assert_eq!(Balances::reserved_balance(1), 0);
	});
}

#[test]
fn timepoint_checking_works() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		let hash = blake2_256(&call);

		assert_noop!(
			Multisig::approve_as_multi(
				Origin::signed(2),
				2,
				vec![1, 3],
				Some(now()),
				hash.clone(),
				0
			),
			Error::<Test>::UnexpectedTimepoint,
		);

		assert_ok!(Multisig::approve_as_multi(Origin::signed(1), 2, vec![2, 3], None, hash, 0));

		assert_noop!(
			Multisig::as_multi(Origin::signed(2), 2, vec![1, 3], None, call.clone(), false, 0),
			Error::<Test>::NoTimepoint,
		);
		let later = Timepoint { index: 1, ..now() };
		assert_noop!(
			Multisig::as_multi(
				Origin::signed(2),
				2,
				vec![1, 3],
				Some(later),
				call.clone(),
				false,
				0
			),
			Error::<Test>::WrongTimepoint,
		);
	});
}

#[test]
fn multisig_2_of_3_works_with_call_storing() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		let hash = blake2_256(&data);
		assert_ok!(Multisig::as_multi(Origin::signed(1), 2, vec![2, 3], None, data, true, 0));
		assert_eq!(Balances::free_balance(6), 0);

		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(2),
			2,
			vec![1, 3],
			Some(now()),
			hash,
			call_weight
		));
		assert_eq!(Balances::free_balance(6), 15);
	});
}

#[test]
fn multisig_2_of_3_works() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		let hash = blake2_256(&data);
		assert_ok!(Multisig::approve_as_multi(Origin::signed(1), 2, vec![2, 3], None, hash, 0));
		assert_eq!(Balances::free_balance(6), 0);

		assert_ok!(Multisig::as_multi(
			Origin::signed(2),
			2,
			vec![1, 3],
			Some(now()),
			data,
			false,
			call_weight
		));
		assert_eq!(Balances::free_balance(6), 15);
	});
}

#[test]
fn multisig_3_of_3_works() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 3);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		let hash = blake2_256(&data);
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			None,
			hash.clone(),
			0
		));
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(2),
			3,
			vec![1, 3],
			Some(now()),
			hash.clone(),
			0
		));
		assert_eq!(Balances::free_balance(6), 0);

		assert_ok!(Multisig::as_multi(
			Origin::signed(3),
			3,
			vec![1, 2],
			Some(now()),
			data,
			false,
			call_weight
		));
		assert_eq!(Balances::free_balance(6), 15);
	});
}

#[test]
fn cancel_multisig_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		let hash = blake2_256(&call);
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			None,
			hash.clone(),
			0
		));
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(2),
			3,
			vec![1, 3],
			Some(now()),
			hash.clone(),
			0
		));
		assert_noop!(
			Multisig::cancel_as_multi(Origin::signed(2), 3, vec![1, 3], now(), hash.clone()),
			Error::<Test>::NotOwner,
		);
		assert_ok!(Multisig::cancel_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			now(),
			hash.clone()
		),);
	});
}

#[test]
fn cancel_multisig_with_call_storage_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		let hash = blake2_256(&call);
		assert_ok!(Multisig::as_multi(Origin::signed(1), 3, vec![2, 3], None, call, true, 0));
		assert_eq!(Balances::free_balance(1), 4);
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(2),
			3,
			vec![1, 3],
			Some(now()),
			hash.clone(),
			0
		));
		assert_noop!(
			Multisig::cancel_as_multi(Origin::signed(2), 3, vec![1, 3], now(), hash.clone()),
			Error::<Test>::NotOwner,
		);
		assert_ok!(Multisig::cancel_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			now(),
			hash.clone()
		),);
		assert_eq!(Balances::free_balance(1), 10);
	});
}

#[test]
fn cancel_multisig_with_alt_call_storage_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		let hash = blake2_256(&call);
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			None,
			hash.clone(),
			0
		));
		assert_eq!(Balances::free_balance(1), 6);
		assert_ok!(Multisig::as_multi(
			Origin::signed(2),
			3,
			vec![1, 3],
			Some(now()),
			call,
			true,
			0
		));
		assert_eq!(Balances::free_balance(2), 8);
		assert_ok!(Multisig::cancel_as_multi(Origin::signed(1), 3, vec![2, 3], now(), hash));
		assert_eq!(Balances::free_balance(1), 10);
		assert_eq!(Balances::free_balance(2), 10);
	});
}

#[test]
fn multisig_2_of_3_as_multi_works() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		assert_ok!(Multisig::as_multi(
			Origin::signed(1),
			2,
			vec![2, 3],
			None,
			data.clone(),
			false,
			0
		));
		assert_eq!(Balances::free_balance(6), 0);

		assert_ok!(Multisig::as_multi(
			Origin::signed(2),
			2,
			vec![1, 3],
			Some(now()),
			data,
			false,
			call_weight
		));
		assert_eq!(Balances::free_balance(6), 15);
	});
}

#[test]
fn multisig_2_of_3_as_multi_with_many_calls_works() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call1 = Call::Balances(BalancesCall::transfer(6, 10));
		let call1_weight = call1.get_dispatch_info().weight;
		let data1 = call1.encode();
		let call2 = Call::Balances(BalancesCall::transfer(7, 5));
		let call2_weight = call2.get_dispatch_info().weight;
		let data2 = call2.encode();

		assert_ok!(Multisig::as_multi(
			Origin::signed(1),
			2,
			vec![2, 3],
			None,
			data1.clone(),
			false,
			0
		));
		assert_ok!(Multisig::as_multi(
			Origin::signed(2),
			2,
			vec![1, 3],
			None,
			data2.clone(),
			false,
			0
		));
		assert_ok!(Multisig::as_multi(
			Origin::signed(3),
			2,
			vec![1, 2],
			Some(now()),
			data1,
			false,
			call1_weight
		));
		assert_ok!(Multisig::as_multi(
			Origin::signed(3),
			2,
			vec![1, 2],
			Some(now()),
			data2,
			false,
			call2_weight
		));

		assert_eq!(Balances::free_balance(6), 10);
		assert_eq!(Balances::free_balance(7), 5);
	});
}

#[test]
fn multisig_2_of_3_cannot_reissue_same_call() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 10));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		let hash = blake2_256(&data);
		assert_ok!(Multisig::as_multi(
			Origin::signed(1),
			2,
			vec![2, 3],
			None,
			data.clone(),
			false,
			0
		));
		assert_ok!(Multisig::as_multi(
			Origin::signed(2),
			2,
			vec![1, 3],
			Some(now()),
			data.clone(),
			false,
			call_weight
		));
		assert_eq!(Balances::free_balance(multi), 5);

		assert_ok!(Multisig::as_multi(
			Origin::signed(1),
			2,
			vec![2, 3],
			None,
			data.clone(),
			false,
			0
		));
		assert_ok!(Multisig::as_multi(
			Origin::signed(3),
			2,
			vec![1, 2],
			Some(now()),
			data.clone(),
			false,
			call_weight
		));

		let err = DispatchError::from(BalancesError::<Test, _>::InsufficientBalance).stripped();
		expect_event(RawEvent::MultisigExecuted(3, now(), multi, hash, Err(err)));
	});
}

#[test]
fn minimum_threshold_check_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		assert_noop!(
			Multisig::as_multi(Origin::signed(1), 0, vec![2], None, call.clone(), false, 0),
			Error::<Test>::MinimumThreshold,
		);
		assert_noop!(
			Multisig::as_multi(Origin::signed(1), 1, vec![2], None, call.clone(), false, 0),
			Error::<Test>::MinimumThreshold,
		);
	});
}

#[test]
fn too_many_signatories_fails() {
	new_test_ext().execute_with(|| {
		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		assert_noop!(
			Multisig::as_multi(Origin::signed(1), 2, vec![2, 3, 4], None, call.clone(), false, 0),
			Error::<Test>::TooManySignatories,
		);
	});
}

#[test]
fn duplicate_approvals_are_ignored() {
	new_test_ext().execute_with(|| {
		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		let hash = blake2_256(&call);
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(1),
			2,
			vec![2, 3],
			None,
			hash.clone(),
			0
		));
		assert_noop!(
			Multisig::approve_as_multi(
				Origin::signed(1),
				2,
				vec![2, 3],
				Some(now()),
				hash.clone(),
				0
			),
			Error::<Test>::AlreadyApproved,
		);
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(2),
			2,
			vec![1, 3],
			Some(now()),
			hash.clone(),
			0
		));
		assert_noop!(
			Multisig::approve_as_multi(
				Origin::signed(3),
				2,
				vec![1, 2],
				Some(now()),
				hash.clone(),
				0
			),
			Error::<Test>::AlreadyApproved,
		);
	});
}

#[test]
fn multisig_1_of_3_works() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 1);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15)).encode();
		let hash = blake2_256(&call);
		assert_noop!(
			Multisig::approve_as_multi(Origin::signed(1), 1, vec![2, 3], None, hash.clone(), 0),
			Error::<Test>::MinimumThreshold,
		);
		assert_noop!(
			Multisig::as_multi(Origin::signed(1), 1, vec![2, 3], None, call.clone(), false, 0),
			Error::<Test>::MinimumThreshold,
		);
		let boxed_call = Box::new(Call::Balances(BalancesCall::transfer(6, 15)));
		assert_ok!(Multisig::as_multi_threshold_1(Origin::signed(1), vec![2, 3], boxed_call));

		assert_eq!(Balances::free_balance(6), 15);
	});
}

#[test]
fn multisig_filters() {
	new_test_ext().execute_with(|| {
		let call = Box::new(Call::System(frame_system::Call::set_code(vec![])));
		assert_noop!(
			Multisig::as_multi_threshold_1(Origin::signed(1), vec![2], call.clone()),
			DispatchError::BadOrigin,
		);
	});
}

#[test]
fn weight_check_works() {
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 2);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let data = call.encode();
		assert_ok!(Multisig::as_multi(
			Origin::signed(1),
			2,
			vec![2, 3],
			None,
			data.clone(),
			false,
			0
		));
		assert_eq!(Balances::free_balance(6), 0);

		assert_noop!(
			Multisig::as_multi(Origin::signed(2), 2, vec![1, 3], Some(now()), data, false, 0),
			Error::<Test>::WeightTooLow,
		);
	});
}

#[test]
fn multisig_handles_no_preimage_after_all_approve() {
	// This test checks the situation where everyone approves a multi-sig, but no-one provides the
	// call data. In the end, any of the multisig callers can approve again with the call data and
	// the call will go through.
	new_test_ext().execute_with(|| {
		let multi = Multisig::multi_account_id(&[1, 2, 3][..], 3);
		assert_ok!(Balances::transfer(Origin::signed(1), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(2), multi, 5));
		assert_ok!(Balances::transfer(Origin::signed(3), multi, 5));

		let call = Call::Balances(BalancesCall::transfer(6, 15));
		let call_weight = call.get_dispatch_info().weight;
		let data = call.encode();
		let hash = blake2_256(&data);
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(1),
			3,
			vec![2, 3],
			None,
			hash.clone(),
			0
		));
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(2),
			3,
			vec![1, 3],
			Some(now()),
			hash.clone(),
			0
		));
		assert_ok!(Multisig::approve_as_multi(
			Origin::signed(3),
			3,
			vec![1, 2],
			Some(now()),
			hash.clone(),
			0
		));
		assert_eq!(Balances::free_balance(6), 0);

		assert_ok!(Multisig::as_multi(
			Origin::signed(3),
			3,
			vec![1, 2],
			Some(now()),
			data,
			false,
			call_weight
		));
		assert_eq!(Balances::free_balance(6), 15);
	});
}
