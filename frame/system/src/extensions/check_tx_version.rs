// This file is part of Substrate.

// Copyright (C) 2017-2020 Parity Technologies (UK) Ltd.
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

use crate::{Module, Trait};
use codec::{Decode, Encode};
use sp_runtime::{traits::SignedExtension, transaction_validity::TransactionValidityError};

/// Ensure the transaction version registered in the transaction is the same as at present.
#[derive(Encode, Decode, Clone, Eq, PartialEq)]
pub struct CheckTxVersion<T: Trait + Send + Sync>(sp_std::marker::PhantomData<T>);

impl<T: Trait + Send + Sync> sp_std::fmt::Debug for CheckTxVersion<T> {
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		write!(f, "CheckTxVersion")
	}

	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		Ok(())
	}
}

impl<T: Trait + Send + Sync> CheckTxVersion<T> {
	/// Create new `SignedExtension` to check transaction version.
	pub fn new() -> Self {
		Self(sp_std::marker::PhantomData)
	}
}

impl<T: Trait + Send + Sync> SignedExtension for CheckTxVersion<T> {
	type AccountId = T::AccountId;
	type Call = <T as Trait>::Call;
	type AdditionalSigned = u32;
	type Pre = ();
	const IDENTIFIER: &'static str = "CheckTxVersion";

	fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
		Ok(<Module<T>>::runtime_version().transaction_version)
	}
}
