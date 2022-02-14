use std::convert::TryInto;
use std::fmt;

use near_contract_standards::storage_management::{
    StorageBalance, StorageBalanceBounds, StorageManagement,
};
use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::collections::{LookupMap, UnorderedSet, Vector};
use near_sdk::json_types::{ValidAccountId, U128};
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{
    assert_one_yocto, env, log, near_bindgen, AccountId, Balance, BorshStorageKey, Gas,
    PanicOnDefault, Promise, PromiseResult, StorageUsage,
};

use crate::account_deposit::{Account, VAccount};
pub use crate::action::SwapAction;
use crate::action::{Action, ActionResult};
use crate::admin_fee::AdminFees;
use crate::aml::{ext_aml, ext_self, AmlOperation};
use crate::errors::*;
use crate::pool::Pool;
use crate::simple_pool::SimplePool;
use crate::stable_swap::StableSwapPool;
use crate::utils::check_token_duplicates;
pub use crate::views::{ContractMetadata, PoolInfo};

const XCC_GAS: Gas = 20_000_000_000_000;

mod account_deposit;
mod action;
mod admin_fee;
mod aml;
mod errors;
mod legacy;
mod multi_fungible_token;
mod owner;
mod pool;
mod simple_pool;
mod stable_swap;
mod storage_impl;
mod token_receiver;
mod utils;
mod views;

near_sdk::setup_alloc!();

#[derive(BorshStorageKey, BorshSerialize)]
pub(crate) enum StorageKey {
    Pools,
    Accounts,
    Shares { pool_id: u32 },
    Whitelist,
    Guardian,
    AccountTokens { account_id: AccountId },
}

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Eq, PartialEq, Clone)]
#[serde(crate = "near_sdk::serde")]
#[cfg_attr(not(target_arch = "wasm32"), derive(Debug))]
pub enum RunningState {
    Running,
    Paused,
}

impl fmt::Display for RunningState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RunningState::Running => write!(f, "Running"),
            RunningState::Paused => write!(f, "Paused"),
        }
    }
}

#[near_bindgen]
#[derive(BorshSerialize, BorshDeserialize, PanicOnDefault)]
pub struct Contract {
    /// Account of the owner.
    owner_id: AccountId,
    /// Exchange fee, that goes to exchange itself (managed by governance).
    exchange_fee: u32,
    /// Referral fee, that goes to referrer in the call.
    referral_fee: u32,
    /// List of all the pools.
    pools: Vector<Pool>,
    /// Accounts registered, keeping track all the amounts deposited, storage and more.
    accounts: LookupMap<AccountId, VAccount>,
    /// Set of whitelisted tokens by "owner".
    whitelisted_tokens: UnorderedSet<AccountId>,
    /// Set of guardians.
    guardians: UnorderedSet<AccountId>,
    /// Running state
    state: RunningState,
    /// Account of an AML contract
    aml_account_id: AccountId,
    /// Accepted risk score (account's risk score should be less or equal to this).
    accepted_risk_score: u8,
}

#[near_bindgen]
impl Contract {
    #[init]
    pub fn new(
        owner_id: ValidAccountId,
        exchange_fee: u32,
        referral_fee: u32,
        aml_account_id: ValidAccountId,
        accepted_risk_score: u8,
    ) -> Self {
        Self {
            owner_id: owner_id.as_ref().clone(),
            exchange_fee,
            referral_fee,
            pools: Vector::new(StorageKey::Pools),
            accounts: LookupMap::new(StorageKey::Accounts),
            whitelisted_tokens: UnorderedSet::new(StorageKey::Whitelist),
            guardians: UnorderedSet::new(StorageKey::Guardian),
            state: RunningState::Running,
            aml_account_id: aml_account_id.as_ref().clone(),
            accepted_risk_score,
        }
    }

    /// Adds new "Simple Pool" with given tokens and given fee.
    /// Attached NEAR should be enough to cover the added storage.
    #[payable]
    pub fn add_simple_pool(&mut self, tokens: Vec<ValidAccountId>, fee: u32) -> u64 {
        self.assert_contract_running();
        check_token_duplicates(&tokens);
        self.internal_add_pool(Pool::SimplePool(SimplePool::new(
            self.pools.len() as u32,
            tokens,
            fee,
            0,
            0,
        )))
    }

    /// Adds new "Stable Pool" with given tokens, decimals, fee and amp.
    /// It is limited to owner or guardians, cause a complex and correct config is needed.
    /// tokens: pool tokens in this stable swap.
    /// decimals: each pool tokens decimal, needed to make them comparable.
    /// fee: total fee of the pool, admin fee is inclusive.
    /// amp_factor: algorithm parameter, decide how stable the pool will be.
    #[payable]
    pub fn add_stable_swap_pool(
        &mut self,
        tokens: Vec<ValidAccountId>,
        decimals: Vec<u8>,
        fee: u32,
        amp_factor: u64,
    ) -> u64 {
        assert!(self.is_owner_or_guardians(), "{}", ERR100_NOT_ALLOWED);
        check_token_duplicates(&tokens);
        self.internal_add_pool(Pool::StableSwapPool(StableSwapPool::new(
            self.pools.len() as u32,
            tokens,
            decimals,
            amp_factor as u128,
            fee,
        )))
    }

    /// [AUDIT_03_reject(NOPE action is allowed by design)]
    /// [AUDIT_04]
    /// Executes generic set of actions.
    /// If referrer provided, pays referral_fee to it.
    /// If no attached deposit, outgoing tokens used in swaps must be whitelisted.
    #[payable]
    pub fn execute_actions(
        &mut self,
        actions: Vec<Action>,
        referral_id: Option<ValidAccountId>,
        sender_id: AccountId,
    ) -> ActionResult {
        self.assert_contract_running();
        let mut account = self.internal_unwrap_account(&sender_id);
        // Validate that all tokens are whitelisted if no deposit (e.g. trade with access key).
        if env::attached_deposit() == 0 {
            for action in &actions {
                for token in action.tokens() {
                    assert!(
                        account.get_balance(&token).is_some()
                            || self.whitelisted_tokens.contains(&token),
                        "{}",
                        // [AUDIT_05]
                        ERR27_DEPOSIT_NEEDED
                    );
                }
            }
        }
        let referral_id = referral_id.map(|r| r.into());
        let result =
            self.internal_execute_actions(&mut account, &referral_id, &actions, ActionResult::None);
        self.internal_save_account(&sender_id, account);
        result
    }

    pub fn update_aml_account_id(&mut self, aml_account_id: ValidAccountId) {
        self.assert_contract_running();
        assert!(self.is_owner_or_guardians(), "{}", ERR100_NOT_ALLOWED);
        self.aml_account_id = aml_account_id.as_ref().clone();
    }

    pub fn update_accepted_risk_score(&mut self, accepted_risk_score: u8) {
        self.assert_contract_running();
        assert!(self.is_owner_or_guardians(), "{}", ERR100_NOT_ALLOWED);
        self.accepted_risk_score = accepted_risk_score;
    }

    #[private]
    #[payable]
    pub fn callback_aml_operation(&mut self, operation: AmlOperation, sender_id: AccountId) {
        match env::promise_result(0) {
            PromiseResult::NotReady => unreachable!(),
            PromiseResult::Failed => env::panic(b"ERR_AML_CALL_FAILED"),
            PromiseResult::Successful(result) => {
                let (category, risk) =
                    near_sdk::serde_json::from_slice::<(String, u8)>(&result).unwrap();
                let is_aml_allowed = if category == "None".to_string() {
                    true
                } else {
                    risk <= self.accepted_risk_score
                };

                self.aml_operation(operation, sender_id, is_aml_allowed);
            }
        }
    }

    #[payable]
    fn aml_operation(
        &mut self,
        operation: AmlOperation,
        sender_id: AccountId,
        is_aml_allowed: bool,
    ) {
        assert!(is_aml_allowed, "ERR_AML_NOT_ALLOWED");
        match operation {
            AmlOperation::Swap {
                actions,
                referral_id,
            } => {
                self.internal_swap_unchecked(actions, referral_id, sender_id);
            }
            AmlOperation::AddLiquidity {
                pool_id,
                amounts,
                min_amounts,
            } => {
                self.internal_add_liquidity_unchecked(pool_id, amounts, sender_id, min_amounts);
            }
            AmlOperation::AddStableLiquidity {
                pool_id,
                amounts,
                min_shares,
            } => {
                self.internal_add_stable_liquidity_unchecked(
                    pool_id, amounts, min_shares, sender_id,
                );
            }
        }
    }

    /// Execute set of swap actions between pools.
    /// If referrer provided, pays referral_fee to it.
    /// If no attached deposit, outgoing tokens used in swaps must be whitelisted.
    #[payable]
    pub fn swap(
        &mut self,
        actions: Vec<SwapAction>,
        referral_id: Option<ValidAccountId>,
    ) -> Promise {
        self.assert_contract_running();
        assert_ne!(actions.len(), 0, "ERR_AT_LEAST_ONE_SWAP");
        ext_aml::get_address(
            env::predecessor_account_id(),
            &self.aml_account_id,
            0,
            XCC_GAS,
        )
        .then(ext_self::callback_aml_operation(
            AmlOperation::Swap {
                actions,
                referral_id,
            },
            env::predecessor_account_id(),
            &env::current_account_id(),
            env::attached_deposit(),
            XCC_GAS,
        ))
    }

    #[payable]
    pub fn add_liquidity(
        &mut self,
        pool_id: u64,
        amounts: Vec<U128>,
        min_amounts: Option<Vec<U128>>,
    ) {
        self.assert_contract_running();
        ext_aml::get_address(
            env::predecessor_account_id(),
            &self.aml_account_id,
            0,
            XCC_GAS,
        )
        .then(ext_self::callback_aml_operation(
            AmlOperation::AddLiquidity {
                pool_id,
                amounts,
                min_amounts,
            },
            env::predecessor_account_id(),
            &env::current_account_id(),
            env::attached_deposit(),
            XCC_GAS,
        ));
    }

    #[payable]
    pub fn add_stable_liquidity(&mut self, pool_id: u64, amounts: Vec<U128>, min_shares: U128) {
        self.assert_contract_running();
        ext_aml::get_address(
            env::predecessor_account_id(),
            &self.aml_account_id,
            0,
            XCC_GAS,
        )
        .then(ext_self::callback_aml_operation(
            AmlOperation::AddStableLiquidity {
                pool_id,
                amounts,
                min_shares,
            },
            env::predecessor_account_id(),
            &env::current_account_id(),
            env::attached_deposit(),
            XCC_GAS,
        ));
    }

    /// Remove liquidity from the pool into general pool of liquidity.
    #[payable]
    pub fn remove_liquidity(&mut self, pool_id: u64, shares: U128, min_amounts: Vec<U128>) {
        assert_one_yocto();
        self.assert_contract_running();
        let prev_storage = env::storage_usage();
        let sender_id = env::predecessor_account_id();
        let mut pool = self.pools.get(pool_id).expect("ERR_NO_POOL");
        let amounts = pool.remove_liquidity(
            &sender_id,
            shares.into(),
            min_amounts
                .into_iter()
                .map(|amount| amount.into())
                .collect(),
        );
        self.pools.replace(pool_id, &pool);
        let tokens = pool.tokens();
        let mut deposits = self.internal_unwrap_or_default_account(&sender_id);
        for i in 0..tokens.len() {
            deposits.deposit(&tokens[i], amounts[i]);
        }
        // Freed up storage balance from LP tokens will be returned to near_balance.
        if prev_storage > env::storage_usage() {
            deposits.near_amount +=
                (prev_storage - env::storage_usage()) as Balance * env::storage_byte_cost();
        }
        self.internal_save_account(&sender_id, deposits);
    }

    /// For stable swap pool, LP can use it to remove liquidity with given token amount and distribution.
    /// pool_id: the stable swap pool id. If simple pool is given, panic with Unimplement.
    /// amounts: Each tokens (in pool tokens sequence) amounts user want get, a 0 means user don't want to get that token back.
    /// max_burn_shares: This is slippage protection, if user request would burn shares more than it, panic with ERR68_SLIPPAGE
    #[payable]
    pub fn remove_liquidity_by_tokens(
        &mut self,
        pool_id: u64,
        amounts: Vec<U128>,
        max_burn_shares: U128,
    ) -> U128 {
        assert_one_yocto();
        self.assert_contract_running();
        let prev_storage = env::storage_usage();
        let sender_id = env::predecessor_account_id();
        let mut pool = self.pools.get(pool_id).expect("ERR_NO_POOL");
        let burn_shares = pool.remove_liquidity_by_tokens(
            &sender_id,
            amounts
                .clone()
                .into_iter()
                .map(|amount| amount.into())
                .collect(),
            max_burn_shares.into(),
            AdminFees::new(self.exchange_fee),
        );
        self.pools.replace(pool_id, &pool);
        let tokens = pool.tokens();
        let mut deposits = self.internal_unwrap_or_default_account(&sender_id);
        for i in 0..tokens.len() {
            deposits.deposit(&tokens[i], amounts[i].into());
        }
        // Freed up storage balance from LP tokens will be returned to near_balance.
        if prev_storage > env::storage_usage() {
            deposits.near_amount +=
                (prev_storage - env::storage_usage()) as Balance * env::storage_byte_cost();
        }
        self.internal_save_account(&sender_id, deposits);

        burn_shares.into()
    }

    /// Add liquidity from already deposited amounts to given pool.
    #[payable]
    fn internal_add_liquidity_unchecked(
        &mut self,
        pool_id: u64,
        amounts: Vec<U128>,
        sender_id: AccountId,
        min_amounts: Option<Vec<U128>>,
    ) {
        assert!(
            env::attached_deposit() > 0,
            "Requires attached deposit of at least 1 yoctoNEAR"
        );
        let prev_storage = env::storage_usage();
        let mut amounts: Vec<u128> = amounts.into_iter().map(|amount| amount.into()).collect();
        let mut pool = self.pools.get(pool_id).expect("ERR_NO_POOL");
        // Add amounts given to liquidity first. It will return the balanced amounts.
        pool.add_liquidity(&sender_id, &mut amounts);
        if let Some(min_amounts) = min_amounts {
            // Check that all amounts are above request min amounts in case of front running that changes the exchange rate.
            for (amount, min_amount) in amounts.iter().zip(min_amounts.iter()) {
                assert!(amount >= &min_amount.0, "ERR_MIN_AMOUNT");
            }
        }
        let mut deposits = self.internal_unwrap_or_default_account(&sender_id);
        let tokens = pool.tokens();
        // Subtract updated amounts from deposits. This will fail if there is not enough funds for any of the tokens.
        for i in 0..tokens.len() {
            deposits.withdraw(&tokens[i], amounts[i]);
        }
        self.internal_save_account(&sender_id, deposits);
        self.pools.replace(pool_id, &pool);
        self.internal_check_storage(prev_storage);
    }

    /// For stable swap pool, user can add liquidity with token's combination as his will.
    /// But there is a little fee according to the bias of token's combination with the one in the pool.
    /// pool_id: stable pool id. If simple pool is given, panic with unimplement.
    /// amounts: token's combination (in pool tokens sequence) user want to add into the pool, a 0 means absent of that token.
    /// min_shares: Slippage, if shares mint is less than it (cause of fee for too much bias), panic with  ERR68_SLIPPAGE
    #[payable]
    fn internal_add_stable_liquidity_unchecked(
        &mut self,
        pool_id: u64,
        amounts: Vec<U128>,
        min_shares: U128,
        sender_id: AccountId,
    ) -> U128 {
        assert!(
            env::attached_deposit() > 0,
            "Requires attached deposit of at least 1 yoctoNEAR"
        );
        let prev_storage = env::storage_usage();
        let amounts: Vec<u128> = amounts.into_iter().map(|amount| amount.into()).collect();
        let mut pool = self.pools.get(pool_id).expect("ERR_NO_POOL");
        // Add amounts given to liquidity first. It will return the balanced amounts.
        let mint_shares = pool.add_stable_liquidity(
            &sender_id,
            &amounts,
            min_shares.into(),
            AdminFees::new(self.exchange_fee),
        );
        let mut deposits = self.internal_unwrap_or_default_account(&sender_id);
        let tokens = pool.tokens();
        // Subtract amounts from deposits. This will fail if there is not enough funds for any of the tokens.
        for i in 0..tokens.len() {
            deposits.withdraw(&tokens[i], amounts[i]);
        }
        self.internal_save_account(&sender_id, deposits);
        self.pools.replace(pool_id, &pool);
        self.internal_check_storage(prev_storage);

        mint_shares.into()
    }
}

/// Internal methods implementation.
impl Contract {
    fn assert_contract_running(&self) {
        match self.state {
            RunningState::Running => (),
            _ => env::panic(ERR51_CONTRACT_PAUSED.as_bytes()),
        };
    }

    /// Check how much storage taken costs and refund the left over back.
    fn internal_check_storage(&self, prev_storage: StorageUsage) {
        let storage_cost = env::storage_usage()
            .checked_sub(prev_storage)
            .unwrap_or_default() as Balance
            * env::storage_byte_cost();

        let refund = env::attached_deposit().checked_sub(storage_cost).expect(
            format!(
                "ERR_STORAGE_DEPOSIT need {}, attatched {}",
                storage_cost,
                env::attached_deposit()
            )
            .as_str(),
        );
        if refund > 0 {
            Promise::new(env::predecessor_account_id()).transfer(refund);
        }
    }

    /// Adds given pool to the list and returns it's id.
    /// If there is not enough attached balance to cover storage, fails.
    /// If too much attached - refunds it back.
    fn internal_add_pool(&mut self, mut pool: Pool) -> u64 {
        let prev_storage = env::storage_usage();
        let id = self.pools.len() as u64;
        // exchange share was registered at creation time
        pool.share_register(&env::current_account_id());
        self.pools.push(&pool);
        self.internal_check_storage(prev_storage);
        id
    }

    /// Execute sequence of actions on given account. Modifies passed account.
    /// Returns result of the last action.
    fn internal_execute_actions(
        &mut self,
        account: &mut Account,
        referral_id: &Option<AccountId>,
        actions: &[Action],
        prev_result: ActionResult,
    ) -> ActionResult {
        let mut result = prev_result;
        for action in actions {
            result = self.internal_execute_action(account, referral_id, action, result);
        }
        result
    }

    /// Executes single action on given account. Modifies passed account. Returns a result based on type of action.
    fn internal_execute_action(
        &mut self,
        account: &mut Account,
        referral_id: &Option<AccountId>,
        action: &Action,
        prev_result: ActionResult,
    ) -> ActionResult {
        match action {
            Action::Swap(swap_action) => {
                let amount_in = swap_action
                    .amount_in
                    .map(|value| value.0)
                    .unwrap_or_else(|| prev_result.to_amount());
                account.withdraw(&swap_action.token_in, amount_in);
                let amount_out = self.internal_pool_swap(
                    swap_action.pool_id,
                    &swap_action.token_in,
                    amount_in,
                    &swap_action.token_out,
                    swap_action.min_amount_out.0,
                    referral_id,
                );
                account.deposit(&swap_action.token_out, amount_out);
                // [AUDIT_02]
                ActionResult::Amount(U128(amount_out))
            }
        }
    }

    /// Swaps given amount_in of token_in into token_out via given pool.
    /// Should be at least min_amount_out or swap will fail (prevents front running and other slippage issues).
    fn internal_pool_swap(
        &mut self,
        pool_id: u64,
        token_in: &AccountId,
        amount_in: u128,
        token_out: &AccountId,
        min_amount_out: u128,
        referral_id: &Option<AccountId>,
    ) -> u128 {
        let mut pool = self.pools.get(pool_id).expect("ERR_NO_POOL");
        let amount_out = pool.swap(
            token_in,
            amount_in,
            token_out,
            min_amount_out,
            AdminFees {
                exchange_fee: self.exchange_fee,
                exchange_id: env::current_account_id(),
                referral_fee: self.referral_fee,
                referral_id: referral_id.clone(),
            },
        );
        self.pools.replace(pool_id, &pool);
        amount_out
    }

    fn internal_swap_unchecked(
        &mut self,
        actions: Vec<SwapAction>,
        referral_id: Option<ValidAccountId>,
        sender_id: AccountId,
    ) -> U128 {
        U128(
            self.execute_actions(
                actions.into_iter().map(Action::Swap).collect(),
                referral_id,
                sender_id,
            )
            .to_amount(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::convert::TryFrom;

    use near_contract_standards::fungible_token::receiver::FungibleTokenReceiver;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::{testing_env, Balance, MockedBlockchain};
    use near_sdk_sim::to_yocto;

    use super::*;

    /// Creates contract and a pool with tokens with 0.3% of total fee.
    fn setup_contract() -> (VMContextBuilder, Contract) {
        let mut context = VMContextBuilder::new();
        testing_env!(context.predecessor_account_id(accounts(0)).build());
        let contract = Contract::new(accounts(0), 1600, 400, accounts(5), 5);
        (context, contract)
    }

    fn deposit_tokens(
        context: &mut VMContextBuilder,
        contract: &mut Contract,
        account_id: ValidAccountId,
        token_amounts: Vec<(ValidAccountId, Balance)>,
    ) {
        if contract.storage_balance_of(account_id.clone()).is_none() {
            testing_env!(context
                .predecessor_account_id(account_id.clone())
                .attached_deposit(to_yocto("1"))
                .build());
            contract.storage_deposit(None, None);
        }
        testing_env!(context
            .predecessor_account_id(account_id.clone())
            .attached_deposit(to_yocto("1"))
            .build());
        let tokens = token_amounts
            .iter()
            .map(|(token_id, _)| token_id.clone().into())
            .collect();
        testing_env!(context.attached_deposit(1).build());
        contract.register_tokens(tokens);
        for (token_id, amount) in token_amounts {
            testing_env!(context
                .predecessor_account_id(token_id)
                .attached_deposit(1)
                .build());
            contract.ft_on_transfer(account_id.clone(), U128(amount), "".to_string());
        }
    }

    fn create_pool_with_liquidity(
        context: &mut VMContextBuilder,
        contract: &mut Contract,
        account_id: ValidAccountId,
        token_amounts: Vec<(ValidAccountId, Balance)>,
    ) -> u64 {
        let tokens = token_amounts
            .iter()
            .map(|(x, _)| x.clone())
            .collect::<Vec<_>>();
        testing_env!(context.predecessor_account_id(accounts(0)).build());
        contract.extend_whitelisted_tokens(tokens.clone());
        testing_env!(context
            .predecessor_account_id(account_id.clone())
            .attached_deposit(env::storage_byte_cost() * 300)
            .build());
        let pool_id = contract.add_simple_pool(tokens, 25);
        testing_env!(context
            .predecessor_account_id(account_id.clone())
            .attached_deposit(to_yocto("0.03"))
            .build());
        contract.storage_deposit(None, None);
        deposit_tokens(context, contract, accounts(3), token_amounts.clone());
        testing_env!(context
            .predecessor_account_id(account_id.clone())
            .attached_deposit(to_yocto("0.0007"))
            .build());
        contract.internal_add_liquidity_unchecked(
            pool_id,
            token_amounts.into_iter().map(|(_, x)| U128(x)).collect(),
            account_id.clone().to_string(),
            None,
        );
        pool_id
    }

    fn swap(
        contract: &mut Contract,
        pool_id: u64,
        token_in: ValidAccountId,
        amount_in: Balance,
        token_out: ValidAccountId,
        sender_id: AccountId,
        min_amount: Option<u128>,
    ) -> U128 {
        contract.internal_swap_unchecked(
            vec![SwapAction {
                pool_id,
                token_in: token_in.into(),
                amount_in: Some(U128(amount_in)),
                token_out: token_out.into(),
                min_amount_out: U128(min_amount.unwrap_or(1)),
            }],
            None,
            sender_id,
        )

        // let promise = contract.swap(
        //     vec![SwapAction {
        //         pool_id,
        //         token_in: token_in.into(),
        //         amount_in: Some(U128(amount_in)),
        //         token_out: token_out.into(),
        //         min_amount_out: U128(1),
        //     }],
        //     None,
        // );
        // todo!("use promises in testing")
    }

    #[test]
    fn test_basics() {
        let one_near = 10u128.pow(24);
        let (mut context, mut contract) = setup_contract();
        // add liquidity of (1,2) tokens
        create_pool_with_liquidity(
            &mut context,
            &mut contract,
            accounts(3),
            vec![(accounts(1), to_yocto("5")), (accounts(2), to_yocto("10"))],
        );
        deposit_tokens(
            &mut context,
            &mut contract,
            accounts(3),
            vec![
                (accounts(1), to_yocto("100")),
                (accounts(2), to_yocto("100")),
            ],
        );
        deposit_tokens(&mut context, &mut contract, accounts(1), vec![]);

        assert_eq!(
            contract.get_deposit(accounts(3), accounts(1)),
            to_yocto("100").into()
        );
        assert_eq!(
            contract.get_deposit(accounts(3), accounts(2)),
            to_yocto("100").into()
        );
        assert_eq!(
            contract.get_pool_total_shares(0).0,
            crate::utils::INIT_SHARES_SUPPLY
        );

        // Get price from pool :0 1 -> 2 tokens.
        let expected_out = contract.get_return(0, accounts(1), one_near.into(), accounts(2));
        assert_eq!(expected_out.0, 1663192997082117548978741);

        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(1)
            .build());
        let amount_out = swap(
            &mut contract,
            0,
            accounts(1),
            one_near,
            accounts(2),
            accounts(3).to_string(),
            None,
        );
        assert_eq!(amount_out.0, expected_out.0);
        assert_eq!(
            contract.get_deposit(accounts(3), accounts(1)).0,
            99 * one_near
        );
        // transfer some of token_id 2 from acc 3 to acc 1.
        testing_env!(context.predecessor_account_id(accounts(3)).build());
        contract.mft_transfer(accounts(2).to_string(), accounts(1), U128(one_near), None);
        assert_eq!(
            contract.get_deposit(accounts(3), accounts(2)).0,
            99 * one_near + amount_out.0
        );
        assert_eq!(contract.get_deposit(accounts(1), accounts(2)).0, one_near);

        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(to_yocto("0.0067"))
            .build());
        contract.mft_register(":0".to_string(), accounts(1));
        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(1)
            .build());
        // transfer 1m shares in pool 0 to acc 1.
        contract.mft_transfer(":0".to_string(), accounts(1), U128(1_000_000), None);

        testing_env!(context.predecessor_account_id(accounts(3)).build());
        contract.remove_liquidity(
            0,
            contract.get_pool_shares(0, accounts(3)),
            vec![1.into(), 2.into()],
        );
        // Exchange fees left in the pool as liquidity + 1m from transfer.
        assert_eq!(
            contract.get_pool_total_shares(0).0,
            33336806279123620258 + 1_000_000
        );

        contract.withdraw(
            accounts(1),
            contract.get_deposit(accounts(3), accounts(1)),
            None,
        );
        assert_eq!(contract.get_deposit(accounts(3), accounts(1)).0, 0);
    }

    /// Test liquidity management.
    #[test]
    fn test_liquidity() {
        let (mut context, mut contract) = setup_contract();
        deposit_tokens(
            &mut context,
            &mut contract,
            accounts(3),
            vec![
                (accounts(1), to_yocto("100")),
                (accounts(2), to_yocto("100")),
            ],
        );
        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(to_yocto("1"))
            .build());
        let id = contract.add_simple_pool(vec![accounts(1), accounts(2)], 25);
        testing_env!(context.attached_deposit(to_yocto("0.0007")).build());
        contract.internal_add_liquidity_unchecked(
            id,
            vec![U128(to_yocto("50")), U128(to_yocto("10"))],
            accounts(3).to_string(),
            None,
        );
        contract.internal_add_liquidity_unchecked(
            id,
            vec![U128(to_yocto("50")), U128(to_yocto("50"))],
            accounts(3).to_string(),
            None,
        );
        testing_env!(context.attached_deposit(1).build());
        contract.remove_liquidity(id, U128(to_yocto("1")), vec![U128(1), U128(1)]);

        // Check that amounts add up to deposits.
        let amounts = contract.get_pool(id).amounts;
        let deposit1 = contract.get_deposit(accounts(3), accounts(1)).0;
        let deposit2 = contract.get_deposit(accounts(3), accounts(2)).0;
        assert_eq!(amounts[0].0 + deposit1, to_yocto("100"));
        assert_eq!(amounts[1].0 + deposit2, to_yocto("100"));
    }

    /// Should deny creating a pool with duplicate tokens.
    #[test]
    #[should_panic(expected = "ERR_TOKEN_DUPLICATES")]
    fn test_deny_duplicate_tokens_pool() {
        let (mut context, mut contract) = setup_contract();
        create_pool_with_liquidity(
            &mut context,
            &mut contract,
            accounts(3),
            vec![(accounts(1), to_yocto("5")), (accounts(1), to_yocto("10"))],
        );
    }

    /// Deny pool with a single token
    #[test]
    #[should_panic(expected = "ERR_SHOULD_HAVE_2_TOKENS")]
    fn test_deny_single_token_pool() {
        let (mut context, mut contract) = setup_contract();
        create_pool_with_liquidity(
            &mut context,
            &mut contract,
            accounts(3),
            vec![(accounts(1), to_yocto("5"))],
        );
    }

    /// Deny pool with a single token
    #[test]
    #[should_panic(expected = "ERR_SHOULD_HAVE_2_TOKENS")]
    fn test_deny_too_many_tokens_pool() {
        let (mut context, mut contract) = setup_contract();
        create_pool_with_liquidity(
            &mut context,
            &mut contract,
            accounts(3),
            vec![
                (accounts(1), to_yocto("5")),
                (accounts(2), to_yocto("10")),
                (accounts(3), to_yocto("10")),
            ],
        );
    }

    #[test]
    #[should_panic(expected = "E12: token not whitelisted")]
    fn test_deny_send_malicious_token() {
        let (mut context, mut contract) = setup_contract();
        let acc = ValidAccountId::try_from("test_user").unwrap();
        testing_env!(context
            .predecessor_account_id(acc.clone())
            .attached_deposit(to_yocto("1"))
            .build());
        contract.storage_deposit(Some(acc.clone()), None);
        testing_env!(context
            .predecessor_account_id(ValidAccountId::try_from("malicious").unwrap())
            .build());
        contract.ft_on_transfer(acc, U128(1_000), "".to_string());
    }

    #[test]
    fn test_send_user_specific_token() {
        let (mut context, mut contract) = setup_contract();
        let acc = ValidAccountId::try_from("test_user").unwrap();
        let custom_token = ValidAccountId::try_from("custom").unwrap();
        testing_env!(context
            .predecessor_account_id(acc.clone())
            .attached_deposit(to_yocto("1"))
            .build());
        contract.storage_deposit(None, None);
        testing_env!(context.attached_deposit(1).build());
        contract.register_tokens(vec![custom_token.clone()]);
        testing_env!(context.predecessor_account_id(custom_token.clone()).build());
        contract.ft_on_transfer(acc.clone(), U128(1_000), "".to_string());
        let prev = contract.storage_balance_of(acc.clone()).unwrap();
        testing_env!(context
            .predecessor_account_id(acc.clone())
            .attached_deposit(1)
            .build());
        contract.withdraw(custom_token, U128(1_000), Some(true));
        let new = contract.storage_balance_of(acc.clone()).unwrap();
        // More available storage after withdrawing & unregistering the token.
        assert!(new.available.0 > prev.available.0);
    }

    #[test]
    #[should_panic(expected = "ERR_MIN_AMOUNT")]
    fn test_deny_min_amount() {
        let (mut context, mut contract) = setup_contract();
        create_pool_with_liquidity(
            &mut context,
            &mut contract,
            accounts(3),
            vec![(accounts(1), to_yocto("1")), (accounts(2), to_yocto("1"))],
        );
        let acc = ValidAccountId::try_from("test_user").unwrap();

        deposit_tokens(
            &mut context,
            &mut contract,
            acc.clone(),
            vec![(accounts(1), 1_000_000)],
        );

        testing_env!(context
            .predecessor_account_id(acc.clone())
            .attached_deposit(1)
            .build());

        swap(
            &mut contract,
            0,
            accounts(1),
            1_000_000,
            accounts(2),
            acc.clone().to_string(),
            Some(1_000_000),
        );
        // contract.swap(
        //     vec![SwapAction {
        //         pool_id: 0,
        //         token_in: accounts(1).into(),
        //         amount_in: Some(U128(1_000_000)),
        //         token_out: accounts(2).into(),
        //         min_amount_out: U128(1_000_000),
        //     }],
        //     None,
        // );
    }

    #[test]
    fn test_second_storage_deposit_works() {
        let (mut context, mut contract) = setup_contract();
        testing_env!(context.attached_deposit(to_yocto("1")).build());
        contract.storage_deposit(None, None);
        testing_env!(context.attached_deposit(to_yocto("0.001")).build());
        contract.storage_deposit(None, None);
    }

    #[test]
    #[should_panic(expected = "ERR_AT_LEAST_ONE_SWAP")]
    fn test_fail_swap_no_actions() {
        let (mut context, mut contract) = setup_contract();
        testing_env!(context.attached_deposit(to_yocto("1")).build());
        contract.storage_deposit(None, None);
        testing_env!(context.attached_deposit(1).build());
        contract.swap(vec![], None);
    }

    /// Check that can not swap non whitelisted tokens when attaching 0 deposit (access key).
    #[test]
    #[ignore]
    #[should_panic(expected = "E27: attach 1yN to swap tokens not in whitelist")]
    fn test_fail_swap_not_whitelisted() {
        let (mut context, mut contract) = setup_contract();
        deposit_tokens(
            &mut context,
            &mut contract,
            accounts(0),
            vec![(accounts(1), 2_000_000), (accounts(2), 1_000_000)],
        );
        create_pool_with_liquidity(
            &mut context,
            &mut contract,
            accounts(0),
            vec![(accounts(1), 1_000_000), (accounts(2), 1_000_000)],
        );
        contract.remove_whitelisted_tokens(vec![accounts(2)]);
        testing_env!(context.attached_deposit(1).build());
        contract.unregister_tokens(vec![accounts(2)]);
        testing_env!(context.attached_deposit(0).build());
        swap(
            &mut contract,
            0,
            accounts(1),
            10,
            accounts(2),
            accounts(3).to_string(),
            None,
        );
    }

    #[test]
    fn test_roundtrip_swap() {
        let (mut context, mut contract) = setup_contract();
        create_pool_with_liquidity(
            &mut context,
            &mut contract,
            accounts(3),
            vec![(accounts(1), to_yocto("5")), (accounts(2), to_yocto("10"))],
        );
        let acc = ValidAccountId::try_from("test_user").unwrap();
        deposit_tokens(
            &mut context,
            &mut contract,
            acc.clone(),
            vec![(accounts(1), 1_000_000)],
        );
        testing_env!(context
            .predecessor_account_id(acc.clone())
            .attached_deposit(1)
            .build());
        contract.internal_swap_unchecked(
            vec![
                SwapAction {
                    pool_id: 0,
                    token_in: accounts(1).into(),
                    amount_in: Some(U128(1_000)),
                    token_out: accounts(2).into(),
                    min_amount_out: U128(1),
                },
                SwapAction {
                    pool_id: 0,
                    token_in: accounts(2).into(),
                    amount_in: None,
                    token_out: accounts(1).into(),
                    min_amount_out: U128(1),
                },
            ],
            None,
            acc.clone().to_string(),
        );
        // Roundtrip returns almost everything except 0.25% fee.
        assert_eq!(contract.get_deposit(acc, accounts(1)).0, 1_000_000 - 6);
    }

    #[test]
    #[should_panic(expected = "E14: LP already registered")]
    fn test_lpt_transfer() {
        // account(0) -- swap contract
        // account(1) -- token0 contract
        // account(2) -- token1 contract
        // account(3) -- user account
        // account(4) -- another user account
        let (mut context, mut contract) = setup_contract();
        deposit_tokens(
            &mut context,
            &mut contract,
            accounts(3),
            vec![
                (accounts(1), to_yocto("100")),
                (accounts(2), to_yocto("100")),
            ],
        );
        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(to_yocto("1"))
            .build());
        let id = contract.add_simple_pool(vec![accounts(1), accounts(2)], 25);
        testing_env!(context.attached_deposit(to_yocto("0.0007")).build());
        contract.internal_add_liquidity_unchecked(
            id,
            vec![U128(to_yocto("50")), U128(to_yocto("10"))],
            accounts(3).to_string(),
            None,
        );
        assert_eq!(
            contract.mft_balance_of(":0".to_string(), accounts(3)).0,
            to_yocto("1")
        );
        assert_eq!(contract.mft_total_supply(":0".to_string()).0, to_yocto("1"));
        testing_env!(context.attached_deposit(1).build());
        contract.internal_add_liquidity_unchecked(
            id,
            vec![U128(to_yocto("50")), U128(to_yocto("50"))],
            accounts(3).to_string(),
            None,
        );
        assert_eq!(
            contract.mft_balance_of(":0".to_string(), accounts(3)).0,
            to_yocto("2")
        );
        assert_eq!(contract.mft_total_supply(":0".to_string()).0, to_yocto("2"));

        // register another user
        testing_env!(context
            .predecessor_account_id(accounts(4))
            .attached_deposit(to_yocto("0.00071"))
            .build());
        contract.mft_register(":0".to_string(), accounts(4));
        // make transfer to him
        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(1)
            .build());
        contract.mft_transfer(":0".to_string(), accounts(4), U128(to_yocto("1")), None);
        assert_eq!(
            contract.mft_balance_of(":0".to_string(), accounts(3)).0,
            to_yocto("1")
        );
        assert_eq!(
            contract.mft_balance_of(":0".to_string(), accounts(4)).0,
            to_yocto("1")
        );
        assert_eq!(contract.mft_total_supply(":0".to_string()).0, to_yocto("2"));
        // remove lpt for account 3
        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(1)
            .build());
        contract.remove_liquidity(id, U128(to_yocto("0.6")), vec![U128(1), U128(1)]);
        assert_eq!(
            contract.mft_balance_of(":0".to_string(), accounts(3)).0,
            to_yocto("0.4")
        );
        assert_eq!(
            contract.mft_total_supply(":0".to_string()).0,
            to_yocto("1.4")
        );
        // remove lpt for account 4 who got lpt from others
        if contract.storage_balance_of(accounts(4)).is_none() {
            testing_env!(context
                .predecessor_account_id(accounts(4))
                .attached_deposit(to_yocto("1"))
                .build());
            contract.storage_deposit(None, None);
        }
        testing_env!(context
            .predecessor_account_id(accounts(4))
            .attached_deposit(1)
            .build());
        contract.remove_liquidity(id, U128(to_yocto("1")), vec![U128(1), U128(1)]);
        assert_eq!(
            contract.mft_balance_of(":0".to_string(), accounts(4)).0,
            to_yocto("0")
        );
        assert_eq!(
            contract.mft_total_supply(":0".to_string()).0,
            to_yocto("0.4")
        );

        // [AUDIT_13]
        // should panic cause accounts(4) not removed by a full remove liquidity
        testing_env!(context
            .predecessor_account_id(accounts(4))
            .attached_deposit(to_yocto("0.00071"))
            .build());
        contract.mft_register(":0".to_string(), accounts(4));
    }

    #[test]
    #[should_panic(expected = "E33: transfer to self")]
    fn test_lpt_transfer_self() {
        // [AUDIT_07]
        // account(0) -- swap contract
        // account(1) -- token0 contract
        // account(2) -- token1 contract
        // account(3) -- user account
        let (mut context, mut contract) = setup_contract();
        deposit_tokens(
            &mut context,
            &mut contract,
            accounts(3),
            vec![
                (accounts(1), to_yocto("100")),
                (accounts(2), to_yocto("100")),
            ],
        );
        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(to_yocto("1"))
            .build());
        let id = contract.add_simple_pool(vec![accounts(1), accounts(2)], 25);
        testing_env!(context.attached_deposit(to_yocto("0.0007")).build());
        contract.internal_add_liquidity_unchecked(
            id,
            vec![U128(to_yocto("50")), U128(to_yocto("10"))],
            accounts(3).to_string(),
            None,
        );
        assert_eq!(
            contract.mft_balance_of(":0".to_string(), accounts(3)).0,
            to_yocto("1")
        );
        testing_env!(context.attached_deposit(1).build());
        contract.internal_add_liquidity_unchecked(
            id,
            vec![U128(to_yocto("50")), U128(to_yocto("50"))],
            accounts(3).to_string(),
            None,
        );
        assert_eq!(
            contract.mft_balance_of(":0".to_string(), accounts(3)).0,
            to_yocto("2")
        );

        // make transfer to self
        testing_env!(context
            .predecessor_account_id(accounts(3))
            .attached_deposit(1)
            .build());
        contract.mft_transfer(":0".to_string(), accounts(3), U128(to_yocto("1")), None);
    }
}
