// This file is part of Substrate.

// Copyright (C) 2017-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::{
	error::{Error, Result},
	wasm_runtime::{RuntimeCache, WasmExecutionMethod},
	RuntimeInfo,
};
use codec::{Decode, Encode};
use log::trace;
use sc_executor_common::wasm_runtime::WasmInstance;
use sp_core::{
	traits::{CodeExecutor, Externalities, MissingHostFunctions, RuntimeCode},
	NativeOrEncoded,
};
use sp_version::{NativeVersion, RuntimeVersion};
use sp_wasm_interface::{Function, HostFunctions};
use std::{
	panic::{AssertUnwindSafe, UnwindSafe},
	result,
	sync::Arc,
};

/// Default num of pages for the heap
const DEFAULT_HEAP_PAGES: u64 = 1024;

/// Set up the externalities and safe calling environment to execute runtime calls.
///
/// If the inner closure panics, it will be caught and return an error.
pub fn with_externalities_safe<F, U>(ext: &mut dyn Externalities, f: F) -> Result<U>
where
	F: UnwindSafe + FnOnce() -> U,
{
	sp_externalities::set_and_run_with_externalities(ext, move || {
		// Substrate uses custom panic hook that terminates process on panic. Disable
		// termination for the native call.
		let _guard = sp_panic_handler::AbortGuard::force_unwind();
		std::panic::catch_unwind(f).map_err(|e| {
			if let Some(err) = e.downcast_ref::<String>() {
				Error::RuntimePanicked(err.clone())
			} else if let Some(err) = e.downcast_ref::<&'static str>() {
				Error::RuntimePanicked(err.to_string())
			} else {
				Error::RuntimePanicked("Unknown panic".into())
			}
		})
	})
}

/// Delegate for dispatching a CodeExecutor call.
///
/// By dispatching we mean that we execute a runtime function specified by it's name.
pub trait NativeExecutionDispatch: Send + Sync {
	/// Host functions for custom runtime interfaces that should be callable from within the runtime
	/// besides the default Substrate runtime interfaces.
	type ExtendHostFunctions: HostFunctions;

	/// Dispatch a method in the runtime.
	///
	/// If the method with the specified name doesn't exist then `Err` is returned.
	fn dispatch(ext: &mut dyn Externalities, method: &str, data: &[u8]) -> Result<Vec<u8>>;

	/// Provide native runtime version.
	fn native_version() -> NativeVersion;
}

/// An abstraction over Wasm code executor. Supports selecting execution backend and
/// manages runtime cache.
#[derive(Clone)]
pub struct WasmExecutor {
	/// Method used to execute fallback Wasm code.
	method: WasmExecutionMethod,
	/// The number of 64KB pages to allocate for Wasm execution.
	default_heap_pages: u64,
	/// The host functions registered with this instance.
	host_functions: Arc<Vec<&'static dyn Function>>,
	/// WASM runtime cache.
	cache: Arc<RuntimeCache>,
	/// The size of the instances cache.
	max_runtime_instances: usize,
}

impl WasmExecutor {
	/// Create new instance.
	///
	/// # Parameters
	///
	/// `method` - Method used to execute Wasm code.
	///
	/// `default_heap_pages` - Number of 64KB pages to allocate for Wasm execution.
	/// 	Defaults to `DEFAULT_HEAP_PAGES` if `None` is provided.
	pub fn new(
		method: WasmExecutionMethod,
		default_heap_pages: Option<u64>,
		host_functions: Vec<&'static dyn Function>,
		max_runtime_instances: usize,
	) -> Self {
		WasmExecutor {
			method,
			default_heap_pages: default_heap_pages.unwrap_or(DEFAULT_HEAP_PAGES),
			host_functions: Arc::new(host_functions),
			cache: Arc::new(RuntimeCache::new(max_runtime_instances)),
			max_runtime_instances,
		}
	}

	/// Execute the given closure `f` with the latest runtime (based on `runtime_code`).
	///
	/// The closure `f` is expected to return `Err(_)` when there happened a `panic!` in native code
	/// while executing the runtime in Wasm. If a `panic!` occurred, the runtime is invalidated to
	/// prevent any poisoned state. Native runtime execution does not need to report back
	/// any `panic!`.
	///
	/// # Safety
	///
	/// `runtime` and `ext` are given as `AssertUnwindSafe` to the closure. As described above, the
	/// runtime is invalidated on any `panic!` to prevent a poisoned state. `ext` is already
	/// implicitly handled as unwind safe, as we store it in a global variable while executing the
	/// native runtime.
	fn with_instance<R, F>(
		&self,
		runtime_code: &RuntimeCode,
		ext: &mut dyn Externalities,
		allow_missing_host_functions: bool,
		f: F,
	) -> Result<R>
	where
		F: FnOnce(
			AssertUnwindSafe<&dyn WasmInstance>,
			Option<&RuntimeVersion>,
			AssertUnwindSafe<&mut dyn Externalities>,
		) -> Result<Result<R>>,
	{
		match self.cache.with_instance(
			runtime_code,
			ext,
			self.method,
			self.default_heap_pages,
			&*self.host_functions,
			allow_missing_host_functions,
			|instance, version, ext| {
				let instance = AssertUnwindSafe(instance);
				let ext = AssertUnwindSafe(ext);
				f(instance, version, ext)
			},
		)? {
			Ok(r) => r,
			Err(e) => Err(e),
		}
	}
}

impl sp_core::traits::CallInWasm for WasmExecutor {
	fn call_in_wasm(
		&self,
		wasm_code: &[u8],
		code_hash: Option<Vec<u8>>,
		method: &str,
		call_data: &[u8],
		ext: &mut dyn Externalities,
		missing_host_functions: MissingHostFunctions,
	) -> std::result::Result<Vec<u8>, String> {
		let allow_missing_host_functions = missing_host_functions.allowed();

		if let Some(hash) = code_hash {
			let code = RuntimeCode {
				code_fetcher: &sp_core::traits::WrappedRuntimeCode(wasm_code.into()),
				hash,
				heap_pages: None,
			};

			self.with_instance(&code, ext, allow_missing_host_functions, |instance, _, mut ext| {
				with_externalities_safe(&mut **ext, move || instance.call(method, call_data))
			})
			.map_err(|e| e.to_string())
		} else {
			let module = crate::wasm_runtime::create_wasm_runtime_with_code(
				self.method,
				self.default_heap_pages,
				&wasm_code,
				self.host_functions.to_vec(),
				allow_missing_host_functions,
			)
			.map_err(|e| format!("Failed to create module: {:?}", e))?;

			let instance =
				module.new_instance().map_err(|e| format!("Failed to create instance: {:?}", e))?;

			let instance = AssertUnwindSafe(instance);
			let mut ext = AssertUnwindSafe(ext);

			with_externalities_safe(&mut **ext, move || instance.call(method, call_data))
				.and_then(|r| r)
				.map_err(|e| e.to_string())
		}
	}
}

/// A generic `CodeExecutor` implementation that uses a delegate to determine wasm code equivalence
/// and dispatch to native code when possible, falling back on `WasmExecutor` when not.
pub struct NativeExecutor<D> {
	/// Dummy field to avoid the compiler complaining about us not using `D`.
	_dummy: std::marker::PhantomData<D>,
	/// Native runtime version info.
	native_version: NativeVersion,
	/// Fallback wasm executor.
	wasm: WasmExecutor,
}

impl<D: NativeExecutionDispatch> NativeExecutor<D> {
	/// Create new instance.
	///
	/// # Parameters
	///
	/// `fallback_method` - Method used to execute fallback Wasm code.
	///
	/// `default_heap_pages` - Number of 64KB pages to allocate for Wasm execution.
	/// 	Defaults to `DEFAULT_HEAP_PAGES` if `None` is provided.
	pub fn new(
		fallback_method: WasmExecutionMethod,
		default_heap_pages: Option<u64>,
		max_runtime_instances: usize,
	) -> Self {
		let mut host_functions = sp_io::SubstrateHostFunctions::host_functions();

		// Add the custom host functions provided by the user.
		host_functions.extend(D::ExtendHostFunctions::host_functions());
		let wasm_executor = WasmExecutor::new(
			fallback_method,
			default_heap_pages,
			host_functions,
			max_runtime_instances,
		);

		NativeExecutor {
			_dummy: Default::default(),
			native_version: D::native_version(),
			wasm: wasm_executor,
		}
	}
}

impl<D: NativeExecutionDispatch> RuntimeInfo for NativeExecutor<D> {
	fn native_version(&self) -> &NativeVersion {
		&self.native_version
	}

	fn runtime_version(
		&self,
		ext: &mut dyn Externalities,
		runtime_code: &RuntimeCode,
	) -> Result<RuntimeVersion> {
		self.wasm.with_instance(runtime_code, ext, false, |_instance, version, _ext| {
			Ok(version.cloned().ok_or_else(|| Error::ApiError("Unknown version".into())))
		})
	}
}

impl<D: NativeExecutionDispatch + 'static> CodeExecutor for NativeExecutor<D> {
	type Error = Error;

	fn call<
		R: Decode + Encode + PartialEq,
		NC: FnOnce() -> result::Result<R, String> + UnwindSafe,
	>(
		&self,
		ext: &mut dyn Externalities,
		runtime_code: &RuntimeCode,
		method: &str,
		data: &[u8],
		use_native: bool,
		native_call: Option<NC>,
	) -> (Result<NativeOrEncoded<R>>, bool) {
		let mut used_native = false;
		let result = self.wasm.with_instance(
			runtime_code,
			ext,
			false,
			|instance, onchain_version, mut ext| {
				let onchain_version =
					onchain_version.ok_or_else(|| Error::ApiError("Unknown version".into()))?;
				match (
					use_native,
					onchain_version.can_call_with(&self.native_version.runtime_version),
					native_call,
				) {
					(_, false, _) => {
						trace!(
							target: "executor",
							"Request for native execution failed (native: {}, chain: {})",
							self.native_version.runtime_version,
							onchain_version,
						);

						with_externalities_safe(&mut **ext, move || {
							instance.call(method, data).map(NativeOrEncoded::Encoded)
						})
					},
					(false, _, _) => with_externalities_safe(&mut **ext, move || {
						instance.call(method, data).map(NativeOrEncoded::Encoded)
					}),
					(true, true, Some(call)) => {
						trace!(
							target: "executor",
							"Request for native execution with native call succeeded \
							(native: {}, chain: {}).",
							self.native_version.runtime_version,
							onchain_version,
						);

						used_native = true;
						let res =
							with_externalities_safe(&mut **ext, move || (call)()).and_then(|r| {
								r.map(NativeOrEncoded::Native).map_err(|s| Error::ApiError(s))
							});

						Ok(res)
					},
					_ => {
						trace!(
							target: "executor",
							"Request for native execution succeeded (native: {}, chain: {})",
							self.native_version.runtime_version,
							onchain_version
						);

						used_native = true;
						Ok(D::dispatch(&mut **ext, method, data).map(NativeOrEncoded::Encoded))
					},
				}
			},
		);
		(result, used_native)
	}
}

impl<D: NativeExecutionDispatch> Clone for NativeExecutor<D> {
	fn clone(&self) -> Self {
		NativeExecutor {
			_dummy: Default::default(),
			native_version: D::native_version(),
			wasm: self.wasm.clone(),
		}
	}
}

impl<D: NativeExecutionDispatch> sp_core::traits::CallInWasm for NativeExecutor<D> {
	fn call_in_wasm(
		&self,
		wasm_blob: &[u8],
		code_hash: Option<Vec<u8>>,
		method: &str,
		call_data: &[u8],
		ext: &mut dyn Externalities,
		missing_host_functions: MissingHostFunctions,
	) -> std::result::Result<Vec<u8>, String> {
		self.wasm.call_in_wasm(wasm_blob, code_hash, method, call_data, ext, missing_host_functions)
	}
}

/// Implements a `NativeExecutionDispatch` for provided parameters.
///
/// # Example
///
/// ```
/// sc_executor::native_executor_instance!(
///     pub MyExecutor,
///     substrate_test_runtime::api::dispatch,
///     substrate_test_runtime::native_version,
/// );
/// ```
///
/// # With custom host functions
///
/// When you want to use custom runtime interfaces from within your runtime, you need to make the
/// executor aware of the host functions for these interfaces.
///
/// ```
/// # use sp_runtime_interface::runtime_interface;
///
/// #[runtime_interface]
/// trait MyInterface {
///     fn say_hello_world(data: &str) {
///         println!("Hello world from: {}", data);
///     }
/// }
///
/// sc_executor::native_executor_instance!(
///     pub MyExecutor,
///     substrate_test_runtime::api::dispatch,
///     substrate_test_runtime::native_version,
///     my_interface::HostFunctions,
/// );
/// ```
///
/// When you have multiple interfaces, you can give the host functions as a tuple e.g.:
/// `(my_interface::HostFunctions, my_interface2::HostFunctions)`
#[macro_export]
macro_rules! native_executor_instance {
	( $pub:vis $name:ident, $dispatcher:path, $version:path $(,)?) => {
		/// A unit struct which implements `NativeExecutionDispatch` feeding in the
		/// hard-coded runtime.
		$pub struct $name;
		$crate::native_executor_instance!(IMPL $name, $dispatcher, $version, ());
	};
	( $pub:vis $name:ident, $dispatcher:path, $version:path, $custom_host_functions:ty $(,)?) => {
		/// A unit struct which implements `NativeExecutionDispatch` feeding in the
		/// hard-coded runtime.
		$pub struct $name;
		$crate::native_executor_instance!(
			IMPL $name, $dispatcher, $version, $custom_host_functions
		);
	};
	(IMPL $name:ident, $dispatcher:path, $version:path, $custom_host_functions:ty) => {
		impl $crate::NativeExecutionDispatch for $name {
			type ExtendHostFunctions = $custom_host_functions;

			fn dispatch(
				ext: &mut dyn $crate::Externalities,
				method: &str,
				data: &[u8]
			) -> $crate::error::Result<Vec<u8>> {
				$crate::with_externalities_safe(ext, move || $dispatcher(method, data))?
					.ok_or_else(|| $crate::error::Error::MethodNotFound(method.to_owned()))
			}

			fn native_version() -> $crate::NativeVersion {
				$version()
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_runtime_interface::runtime_interface;

	#[runtime_interface]
	trait MyInterface {
		fn say_hello_world(data: &str) {
			println!("Hello world from: {}", data);
		}
	}

	native_executor_instance!(
		pub MyExecutor,
		substrate_test_runtime::api::dispatch,
		substrate_test_runtime::native_version,
		(my_interface::HostFunctions, my_interface::HostFunctions),
	);

	#[test]
	fn native_executor_registers_custom_interface() {
		let executor = NativeExecutor::<MyExecutor>::new(WasmExecutionMethod::Interpreted, None, 8);
		my_interface::HostFunctions::host_functions().iter().for_each(|function| {
			assert_eq!(executor.wasm.host_functions.iter().filter(|f| f == &function).count(), 2,);
		});

		my_interface::say_hello_world("hey");
	}
}
