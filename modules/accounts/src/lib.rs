//! # Accounts Module
//!
//! ## Overview
//!
//! Accounts module is responsible for opening and closing accounts in Acala,
//! and charge fee and tip in different currencies

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use frame_support::{
	decl_error, decl_module, decl_storage,
	dispatch::{DispatchResult, Dispatchable},
	ensure,
	traits::{
		Currency, ExistenceRequirement, Get, Happened, Imbalance, OnKilledAccount, OnUnbalanced, StoredMap,
		WithdrawReason,
	},
	weights::{DispatchInfo, PostDispatchInfo, Weight},
	IsSubType,
};
use frame_system::{self as system, ensure_signed, AccountInfo};
use orml_traits::{MultiCurrency, MultiLockableCurrency, MultiReservableCurrency, OnReceived};
use orml_utilities::with_transaction_result;
use primitives::{Balance, CurrencyId};
use sp_runtime::{
	traits::{
		AccountIdConversion, CheckedSub, DispatchInfoOf, PostDispatchInfoOf, SaturatedConversion, Saturating,
		SignedExtension, UniqueSaturatedInto, Zero,
	},
	transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionValidity, TransactionValidityError, ValidTransaction,
	},
	FixedPointOperand, ModuleId,
};
use sp_std::convert::Infallible;
use sp_std::{prelude::*, vec};
use support::{DEXManager, Ratio};

mod default_weight;
mod mock;
mod tests;

pub trait WeightInfo {
	fn close_account(c: u32) -> Weight;
}

type PalletBalanceOf<T> =
	<<T as pallet_transaction_payment::Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::Balance;
type NegativeImbalanceOf<T> = <<T as pallet_transaction_payment::Trait>::Currency as Currency<
	<T as system::Trait>::AccountId,
>>::NegativeImbalance;

pub trait Trait: system::Trait + pallet_transaction_payment::Trait + orml_currencies::Trait {
	/// All non-native currency ids in Acala.
	type AllNonNativeCurrencyIds: Get<Vec<CurrencyId>>;

	/// Native currency id, the actual received currency type as fee for
	/// treasury. Should be ACA
	type NativeCurrencyId: Get<CurrencyId>;

	/// Stable currency id, should be AUSD
	type StableCurrencyId: Get<CurrencyId>;

	/// Currency to transfer, reserve/unreserve, lock/unlock assets
	type Currency: MultiLockableCurrency<Self::AccountId, Moment = Self::BlockNumber, CurrencyId = CurrencyId, Balance = Balance>
		+ MultiReservableCurrency<Self::AccountId, CurrencyId = CurrencyId, Balance = Balance>;

	/// DEX to exchange currencies.
	type DEX: DEXManager<Self::AccountId, CurrencyId, Balance>;

	/// Handler to trigger events when opening accounts

	/// Handler for the unbalanced reduction when taking transaction fees. This
	/// is either one or two separate imbalances, the first is the transaction
	/// fee paid, the second is the tip paid, if any.

	/// Event handler which calls when open account in system.
	type OnCreatedAccount: Happened<Self::AccountId>;

	/// Handler to kill account in system.
	type KillAccount: Happened<Self::AccountId>;

	/// Deposit for opening account, would be reserved until account closed.
	type NewAccountDeposit: Get<Balance>;

	/// The treasury module account id to recycle assets.
	type TreasuryModuleId: Get<ModuleId>;

	/// The max slippage allowed when swap open account deposit or fee with DEX
	type MaxSlippageSwapWithDEX: Get<Ratio>;

	/// Weight information for the extrinsics in this module.
	type WeightInfo: WeightInfo;
}

decl_error! {
	/// Error for accounts manager module.
	pub enum Error for Module<T: Trait> {
		/// Balance is not sufficient
		NotEnoughBalance,
		/// Account ref count is not zero
		NonZeroRefCount,
		/// Account still has active reserved(include non-native token and native token beyond new account deposit)
		StillHasActiveReserved,
	}
}

decl_storage! {
	trait Store for Module<T: Trait> as Accounts {}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		type Error = Error<T>;

		/// All non-native currency ids in Acala.
		const AllNonNativeCurrencyIds: Vec<CurrencyId> = T::AllNonNativeCurrencyIds::get();

		/// Native currency id, the actual received currency type as fee for treasury.
		const NativeCurrencyId: CurrencyId = T::NativeCurrencyId::get();

		/// Stable currency id.
		const StableCurrencyId: CurrencyId = T::StableCurrencyId::get();

		/// Deposit for opening account, would be reserved until account closed.
		const NewAccountDeposit: Balance = T::NewAccountDeposit::get();

		/// The treasury module account id to recycle assets.
		const TreasuryModuleId: ModuleId = T::TreasuryModuleId::get();

		/// The max slippage allowed when swap open account deposit or fee with DEX
		const MaxSlippageSwapWithDEX: Ratio = T::MaxSlippageSwapWithDEX::get();

		/// Kill self account from system.
		///
		/// The dispatch origin of this call must be Signed.
		///
		/// - `recipient`: the account as recipient to receive remaining currencies of the account will be killed,
		///					None means no recipient is specified.
		#[weight = <T as Trait>::WeightInfo::close_account(T::AllNonNativeCurrencyIds::get().len() as u32)]
		pub fn close_account(origin, recipient: Option<T::AccountId>) {
			with_transaction_result(|| {
				let who = ensure_signed(origin)?;

				// check must allow death,
				// if native/non-native currencies has locks, means ref_count shouldn't be zero, can not close the account.
				ensure!(
					<system::Module<T>>::allow_death(&who),
					Error::<T>::NonZeroRefCount,
				);

				let native_currency_id = T::NativeCurrencyId::get();
				let new_account_deposit = T::NewAccountDeposit::get();
				let total_reserved_native = <T as Trait>::Currency::reserved_balance(native_currency_id, &who);

				// ensure total reserved native is lte new account deposit,
				// otherwise think the account still has active reserved kept by some bussiness.
				ensure!(
					new_account_deposit >= total_reserved_native,
					Error::<T>::StillHasActiveReserved,
				);
				let treasury_account = Self::treasury_account_id();
				let recipient = recipient.unwrap_or_else(|| treasury_account.clone());

				// unreserve all reserved native currency
				<T as Trait>::Currency::unreserve(native_currency_id, &who, total_reserved_native);

				// transfer all free to recipient
				<T as Trait>::Currency::transfer(native_currency_id, &who, &recipient, <T as Trait>::Currency::free_balance(native_currency_id, &who))?;

				// handle other non-native currencies
				for currency_id in T::AllNonNativeCurrencyIds::get() {
					// ensure the account has no active reserved of non-native token
					ensure!(
						<T as Trait>::Currency::reserved_balance(currency_id, &who).is_zero(),
						Error::<T>::StillHasActiveReserved,
					);

					// transfer all free to recipient
					<T as Trait>::Currency::transfer(currency_id, &who, &recipient, <T as Trait>::Currency::free_balance(currency_id, &who))?;
				}

				// finally kill the account
				T::KillAccount::happened(&who);

				Ok(())
			})?;
		}
	}
}

impl<T: Trait> Module<T> {
	/// Get treasury account id.
	pub fn treasury_account_id() -> T::AccountId {
		T::TreasuryModuleId::get().into_account()
	}

	/// Open account by reserve native token.
	///
	/// If not enough free balance to reserve, all the balance would be
	/// transferred to treasury instead.
	fn open_account(k: &T::AccountId) {
		let native_currency_id = T::NativeCurrencyId::get();
		if <T as Trait>::Currency::reserve(native_currency_id, k, T::NewAccountDeposit::get()).is_ok() {
			T::OnCreatedAccount::happened(&k);
		} else {
			let treasury_account = Self::treasury_account_id();

			// Note: will not reap treasury account even though it cannot reserve open
			// account deposit best practice is to ensure that the first transfer received
			// by treasury account is sufficient to open an account.
			if *k != treasury_account {
				// send dust native currency to treasury account.
				// transfer all free balances from a new account to treasury account, so it
				// shouldn't fail. but even it failed, leave some dust storage is not a critical
				// issue, just open account without reserve NewAccountDeposit.
				if <T as Trait>::Currency::transfer(
					native_currency_id,
					k,
					&treasury_account,
					<T as Trait>::Currency::free_balance(native_currency_id, k),
				)
				.is_ok()
				{
					// remove the account info pretend that opening account has never happened
					system::Account::<T>::remove(k);
				}
			}
		}
	}
}

/// Note: Currently `pallet_balances` does not implement `OnReceived`,
/// which means here only do the preparations for opening an account by
/// non-native currency, actual process of opening account is handled by
/// `StoredMap`.
impl<T: Trait> OnReceived<T::AccountId, CurrencyId, Balance> for Module<T> {
	fn on_received(who: &T::AccountId, currency_id: CurrencyId, _: Balance) {
		let native_currency_id = T::NativeCurrencyId::get();

		if !<Self as StoredMap<_, _>>::is_explicit(who) && currency_id != native_currency_id {
			let stable_currency_id = T::StableCurrencyId::get();
			let trading_path = if currency_id == stable_currency_id {
				vec![stable_currency_id, native_currency_id]
			} else {
				vec![currency_id, stable_currency_id, native_currency_id]
			};

			// Successful swap will cause changes in native currency,
			// which also means that it will open a new account
			// exchange token to native currency and open account.
			// If swap failed, will leave some dust storage is not a critical issue,
			// just open account without reserve NewAccountDeposit.
			// Don't recycle non-native to avoid unreasonable loss
			// due to insufficient liquidity of DEX, can try to open this
			// account again later. If want to recycle dust non-native,
			// should handle by the currencies module.
			let _ = T::DEX::swap_with_exact_target(
				who,
				&trading_path,
				T::NewAccountDeposit::get(),
				<T as Trait>::Currency::free_balance(currency_id, who),
				Some(T::MaxSlippageSwapWithDEX::get()),
			);
		}
	}
}

/// Fork StoredMap in frame_system,  still use `Account` storage of
/// frame_system.
impl<T: Trait> StoredMap<T::AccountId, T::AccountData> for Module<T> {
	fn get(k: &T::AccountId) -> T::AccountData {
		system::Account::<T>::get(k).data
	}

	fn is_explicit(k: &T::AccountId) -> bool {
		system::Account::<T>::contains_key(k)
	}

	fn insert(k: &T::AccountId, data: T::AccountData) {
		let existed = system::Account::<T>::contains_key(k);
		system::Account::<T>::mutate(k, |a| a.data = data);
		// if not existed before, create new account info
		if !existed {
			Self::open_account(k);
		}
	}

	fn remove(k: &T::AccountId) {
		T::KillAccount::happened(k);
	}

	fn mutate<R>(k: &T::AccountId, f: impl FnOnce(&mut T::AccountData) -> R) -> R {
		let existed = system::Account::<T>::contains_key(k);
		let r = system::Account::<T>::mutate(k, |a| f(&mut a.data));
		if !existed {
			T::OnCreatedAccount::happened(&k);
		}
		r
	}

	fn mutate_exists<R>(k: &T::AccountId, f: impl FnOnce(&mut Option<T::AccountData>) -> R) -> R {
		Self::try_mutate_exists(k, |x| -> Result<R, Infallible> { Ok(f(x)) }).expect("Infallible; qed")
	}

	fn try_mutate_exists<R, E>(
		k: &T::AccountId,
		f: impl FnOnce(&mut Option<T::AccountData>) -> Result<R, E>,
	) -> Result<R, E> {
		system::Account::<T>::try_mutate_exists(k, |maybe_value| {
			let existed = maybe_value.is_some();
			let (maybe_prefix, mut maybe_data) = split_inner(maybe_value.take(), |account| {
				((account.nonce, account.refcount), account.data)
			});
			f(&mut maybe_data).map(|result| {
				// Note: do not remove the AccountData storage even if the maybe_data is None
				let (nonce, refcount) = maybe_prefix.unwrap_or_default();
				let data = maybe_data.unwrap_or_default();
				*maybe_value = Some(AccountInfo { nonce, refcount, data });

				(existed, maybe_value.is_some(), result)
			})
		})
		.map(|(existed, exists, v)| {
			if !existed && exists {
				// need to open account
				Self::open_account(k);
			}

			v
		})
	}
}

/// Split an `option` into two constituent options, as defined by a `splitter`
/// function.
pub fn split_inner<T, R, S>(option: Option<T>, splitter: impl FnOnce(T) -> (R, S)) -> (Option<R>, Option<S>) {
	match option {
		Some(inner) => {
			let (r, s) = splitter(inner);
			(Some(r), Some(s))
		}
		None => (None, None),
	}
}

impl<T: Trait> OnKilledAccount<T::AccountId> for Module<T> {
	fn on_killed_account(_who: &T::AccountId) {}
}

/// Require the transactor pay for themselves and maybe include a tip to gain
/// additional priority in the queue.
#[derive(Encode, Decode, Clone, Eq, PartialEq)]
pub struct ChargeTransactionPayment<T: Trait + Send + Sync>(#[codec(compact)] PalletBalanceOf<T>);

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

impl<T: Trait + Send + Sync> ChargeTransactionPayment<T>
where
	T::Call: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo> + IsSubType<orml_currencies::Call<T>>,
	PalletBalanceOf<T>: Send + Sync + FixedPointOperand,
{
	/// utility constructor. Used only in client/factory code.
	pub fn from(fee: PalletBalanceOf<T>) -> Self {
		Self(fee)
	}

	fn withdraw_fee(
		&self,
		who: &T::AccountId,
		_call: &T::Call,
		info: &DispatchInfoOf<T::Call>,
		len: usize,
	) -> Result<(PalletBalanceOf<T>, Option<NegativeImbalanceOf<T>>), TransactionValidityError> {
		// pay any fees.
		let tip = self.0;
		let fee = <pallet_transaction_payment::Module<T>>::compute_fee(len as u32, info, tip);

		let reason = if tip.is_zero() {
			WithdrawReason::TransactionPayment.into()
		} else {
			WithdrawReason::TransactionPayment | WithdrawReason::Tip
		};

		// check native balance if is enough
		let native_is_enough = <T as pallet_transaction_payment::Trait>::Currency::free_balance(who)
			.checked_sub(&fee)
			.map_or(false, |new_free_balance| {
				<T as pallet_transaction_payment::Trait>::Currency::ensure_can_withdraw(
					who,
					fee,
					reason,
					new_free_balance,
				)
				.is_ok()
			});

		// try to use non-native currency to swap native currency by exchange with DEX
		if !native_is_enough {
			let native_currency_id = T::NativeCurrencyId::get();
			let stable_currency_id = T::StableCurrencyId::get();
			let other_currency_ids = T::AllNonNativeCurrencyIds::get();
			let price_impact_limit = Some(T::MaxSlippageSwapWithDEX::get());
			// Note: in fact, just obtain the gap between of fee and usable native currency
			// amount, but `Currency` does not expose interface to get usable balance by
			// specific reason. Here try to swap the whole fee by non-native currency.
			let balance_fee: Balance = fee.unique_saturated_into();

			// iterator non-native currencies to get enough fee
			for currency_id in other_currency_ids {
				let trading_path = if currency_id == stable_currency_id {
					vec![stable_currency_id, native_currency_id]
				} else {
					vec![currency_id, stable_currency_id, native_currency_id]
				};

				if T::DEX::swap_with_exact_target(
					who,
					&trading_path,
					balance_fee,
					<T as Trait>::Currency::free_balance(currency_id, who),
					price_impact_limit,
				)
				.is_ok()
				{
					// successfully swap, break iteration
					break;
				}
			}
		}

		// withdraw native currency as fee
		match <T as pallet_transaction_payment::Trait>::Currency::withdraw(
			who,
			fee,
			reason,
			ExistenceRequirement::KeepAlive,
		) {
			Ok(imbalance) => Ok((fee, Some(imbalance))),
			Err(_) => Err(InvalidTransaction::Payment.into()),
		}
	}

	/// Get an appropriate priority for a transaction with the given length and
	/// info.
	///
	/// This will try and optimise the `fee/weight` `fee/length`, whichever is
	/// consuming more of the maximum corresponding limit.
	///
	/// For example, if a transaction consumed 1/4th of the block length and
	/// half of the weight, its final priority is `fee * min(2, 4) = fee * 2`.
	/// If it consumed `1/4th` of the block length and the entire block weight
	/// `(1/1)`, its priority is `fee * min(1, 4) = fee * 1`. This means
	///  that the transaction which consumes more resources (either length or
	/// weight) with the same `fee` ends up having lower priority.
	fn get_priority(len: usize, info: &DispatchInfoOf<T::Call>, final_fee: PalletBalanceOf<T>) -> TransactionPriority {
		let weight_saturation = T::MaximumBlockWeight::get() / info.weight.max(1);
		let len_saturation = T::MaximumBlockLength::get() as u64 / (len as u64).max(1);
		let coefficient: PalletBalanceOf<T> = weight_saturation
			.min(len_saturation)
			.saturated_into::<PalletBalanceOf<T>>();
		final_fee
			.saturating_mul(coefficient)
			.saturated_into::<TransactionPriority>()
	}
}

impl<T: Trait + Send + Sync> SignedExtension for ChargeTransactionPayment<T>
where
	PalletBalanceOf<T>: Send + Sync + From<u64> + FixedPointOperand,
	T::Call: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo> + IsSubType<orml_currencies::Call<T>>,
{
	const IDENTIFIER: &'static str = "ChargeTransactionPayment";
	type AccountId = T::AccountId;
	type Call = T::Call;
	type AdditionalSigned = ();
	type Pre = (
		PalletBalanceOf<T>,
		Self::AccountId,
		Option<NegativeImbalanceOf<T>>,
		PalletBalanceOf<T>,
	);

	fn additional_signed(&self) -> sp_std::result::Result<(), TransactionValidityError> {
		Ok(())
	}

	fn validate(
		&self,
		who: &Self::AccountId,
		call: &Self::Call,
		info: &DispatchInfoOf<Self::Call>,
		len: usize,
	) -> TransactionValidity {
		let (fee, _) = self.withdraw_fee(who, call, info, len)?;
		Ok(ValidTransaction {
			priority: Self::get_priority(len, info, fee),
			..Default::default()
		})
	}

	fn pre_dispatch(
		self,
		who: &Self::AccountId,
		call: &Self::Call,
		info: &DispatchInfoOf<Self::Call>,
		len: usize,
	) -> Result<Self::Pre, TransactionValidityError> {
		let (fee, imbalance) = self.withdraw_fee(who, call, info, len)?;
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
			let actual_fee =
				<pallet_transaction_payment::Module<T>>::compute_actual_fee(len as u32, info, post_info, tip);
			let refund = fee.saturating_sub(actual_fee);
			let actual_payment =
				match <T as pallet_transaction_payment::Trait>::Currency::deposit_into_existing(&who, refund) {
					Ok(refund_imbalance) => {
						// The refund cannot be larger than the up front payed max weight.
						// `PostDispatchInfo::calc_unspent` guards against such a case.
						match payed.offset(refund_imbalance) {
							Ok(actual_payment) => actual_payment,
							Err(_) => return Err(InvalidTransaction::Payment.into()),
						}
					}
					// We do not recreate the account using the refund. The up front payment
					// is gone in that case.
					Err(_) => payed,
				};
			let imbalances = actual_payment.split(tip);

			// distribute fee by `pallet_transaction_payment`
			<T as pallet_transaction_payment::Trait>::OnTransactionPayment::on_unbalanceds(
				Some(imbalances.0).into_iter().chain(Some(imbalances.1)),
			);
		}
		Ok(())
	}
}
