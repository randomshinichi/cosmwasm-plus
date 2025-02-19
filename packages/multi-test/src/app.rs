use serde::Serialize;

#[cfg(test)]
use cosmwasm_std::testing::{mock_env, MockApi};
use cosmwasm_std::{
    from_slice, to_binary, to_vec, Addr, Api, Attribute, BankMsg, Binary, BlockInfo, Coin,
    ContractResult, CosmosMsg, Empty, MessageInfo, Querier, QuerierResult, QuerierWrapper,
    QueryRequest, Response, SubMsg, SystemError, SystemResult, WasmMsg,
};

use crate::bank::{Bank, BankCache, BankOps, BankRouter};
use crate::wasm::{Contract, StorageFactory, WasmCache, WasmOps, WasmRouter};
use schemars::JsonSchema;
use std::fmt;

#[derive(Default, Clone, Debug)]
pub struct AppResponse {
    pub attributes: Vec<Attribute>,
    pub data: Option<Binary>,
}

// This can be Response, Response, MigrationResponse
#[derive(Default, Clone)]
pub struct ActionResponse<C>
where
    C: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    pub messages: Vec<CosmosMsg<C>>,
    pub attributes: Vec<Attribute>,
    pub data: Option<Binary>,
}

impl<C> From<Response<C>> for ActionResponse<C>
where
    C: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    fn from(input: Response<C>) -> Self {
        ActionResponse {
            messages: input.messages.into_iter().map(|m| m.msg).collect(),
            attributes: input.attributes,
            data: input.data,
        }
    }
}

impl<C> ActionResponse<C>
where
    C: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    fn init(input: Response<C>, address: Addr) -> Self {
        ActionResponse {
            messages: input.messages.into_iter().map(|m| m.msg).collect(),
            attributes: input.attributes,
            data: Some(address.as_ref().as_bytes().into()),
        }
    }
}

impl<C> Querier for App<C>
where
    C: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    fn raw_query(&self, bin_request: &[u8]) -> QuerierResult {
        // TODO: we need to make a new custom type for queries
        let request: QueryRequest<Empty> = match from_slice(bin_request) {
            Ok(v) => v,
            Err(e) => {
                return SystemResult::Err(SystemError::InvalidRequest {
                    error: format!("Parsing query request: {}", e.to_string()),
                    request: bin_request.into(),
                })
            }
        };
        let contract_result: ContractResult<Binary> = self.query(request).into();
        SystemResult::Ok(contract_result)
    }
}

/// Router is a persisted state. You can query this.
/// Execution generally happens on the RouterCache, which then can be atomically committed or rolled back.
/// We offer .execute() as a wrapper around cache, execute, commit/rollback process.
///
/// C is the custom message returned init, handle, sudo (Response<C>).
/// All contracts must return Response<C> or Response<Empty>
pub struct App<C = Empty>
where
    C: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    wasm: WasmRouter<C>,
    bank: BankRouter,
}

impl<C> App<C>
where
    C: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    pub fn new<B: Bank + 'static>(
        api: Box<dyn Api>,
        block: BlockInfo,
        bank: B,
        storage_factory: StorageFactory,
    ) -> Self {
        App {
            wasm: WasmRouter::new(api, block, storage_factory),
            bank: BankRouter::new(bank, storage_factory()),
        }
    }

    pub fn cache(&'_ self) -> AppCache<'_, C> {
        AppCache::new(self)
    }

    /// This can set the block info to any value. Must be done before taking a cache
    pub fn set_block(&mut self, block: BlockInfo) {
        self.wasm.set_block(block);
    }

    /// This let's use use "next block" steps that add eg. one height and 5 seconds
    pub fn update_block<F: Fn(&mut BlockInfo)>(&mut self, action: F) {
        self.wasm.update_block(action);
    }

    /// Returns a copy of the current block_info
    pub fn block_info(&self) -> BlockInfo {
        self.wasm.block_info()
    }

    /// This is an "admin" function to let us adjust bank accounts
    pub fn set_bank_balance(&mut self, account: &Addr, amount: Vec<Coin>) -> Result<(), String> {
        self.bank.set_balance(account, amount)
    }

    /// This registers contract code (like uploading wasm bytecode on a chain),
    /// so it can later be used to instantiate a contract.
    pub fn store_code(&mut self, code: Box<dyn Contract<C>>) -> u64 {
        self.wasm.store_code(code) as u64
    }

    /// Simple helper so we get access to all the QuerierWrapper helpers,
    /// eg. query_wasm_smart, query_all_balances, ...
    pub fn wrap(&self) -> QuerierWrapper {
        QuerierWrapper::new(self)
    }

    /// Handles arbitrary QueryRequest, this is wrapped by the Querier interface, but this
    /// is nicer to use.
    pub fn query(&self, request: QueryRequest<Empty>) -> Result<Binary, String> {
        match request {
            QueryRequest::Wasm(req) => self.wasm.query(self, req),
            QueryRequest::Bank(req) => self.bank.query(req),
            _ => unimplemented!(),
        }
    }

    /// Create a contract and get the new address.
    /// This is just a helper around execute()
    pub fn instantiate_contract<T: Serialize, U: Into<String>>(
        &mut self,
        code_id: u64,
        sender: Addr,
        init_msg: &T,
        send_funds: &[Coin],
        label: U,
    ) -> Result<Addr, String> {
        // instantiate contract
        let init_msg = to_binary(init_msg).map_err(|e| e.to_string())?;
        let msg = SubMsg::new(WasmMsg::Instantiate {
            admin: None,
            code_id,
            msg: init_msg,
            funds: send_funds.to_vec(),
            label: label.into(),
        });
        let res = self.execute(sender, msg)?;
        parse_contract_addr(&res.data)
    }

    /// Execute a contract and process all returned messages.
    /// This is just a helper around execute()
    pub fn execute_contract<T: Serialize>(
        &mut self,
        sender: Addr,
        contract_addr: Addr,
        msg: &T,
        send_funds: &[Coin],
    ) -> Result<AppResponse, String> {
        let msg = to_binary(msg).map_err(|e| e.to_string())?;
        let msg = SubMsg::new(WasmMsg::Execute {
            contract_addr: contract_addr.into(),
            msg,
            funds: send_funds.to_vec(),
        });
        self.execute(sender, msg)
    }

    /// Runs arbitrary CosmosMsg.
    /// This will create a cache before the execution, so no state changes are persisted if this
    /// returns an error, but all are persisted on success.
    pub fn execute(&mut self, sender: Addr, msg: SubMsg<C>) -> Result<AppResponse, String> {
        let mut all = self.execute_multi(sender, vec![msg])?;
        let res = all.pop().unwrap();
        Ok(res)
    }

    /// Runs multiple CosmosMsg in one atomic operation.
    /// This will create a cache before the execution, so no state changes are persisted if any of them
    /// return an error. But all writes are persisted on success.
    pub fn execute_multi(
        &mut self,
        sender: Addr,
        msgs: Vec<SubMsg<C>>,
    ) -> Result<Vec<AppResponse>, String> {
        // we need to do some caching of storage here, once in the entry point:
        // meaning, wrap current state, all writes go to a cache, only when execute
        // returns a success do we flush it (otherwise drop it)

        let mut cache = self.cache();

        // run all messages, stops at first error
        let res: Result<Vec<AppResponse>, String> = msgs
            .into_iter()
            .map(|msg| cache.execute(sender.clone(), msg))
            .collect();

        // this only happens if all messages run successfully
        if res.is_ok() {
            let ops = cache.prepare();
            ops.commit(self);
        }
        res
    }

    /// Runs arbitrary CosmosMsg.
    /// This will create a cache before the execution, so no state changes are persisted if this
    /// returns an error, but all are persisted on success.
    pub fn sudo<T: Serialize, U: Into<Addr>>(
        &mut self,
        contract_addr: U,
        msg: &T,
    ) -> Result<AppResponse, String> {
        let msg = to_vec(msg).map_err(|e| e.to_string())?;
        let mut cache = self.cache();

        let res = cache.sudo(contract_addr.into(), msg);

        // this only happens if all messages run successfully
        if res.is_ok() {
            let ops = cache.prepare();
            ops.commit(self);
        }
        res
    }
}

pub struct AppCache<'a, C>
where
    C: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    router: &'a App<C>,
    wasm: WasmCache<'a, C>,
    bank: BankCache<'a>,
}

pub struct AppOps {
    wasm: WasmOps,
    bank: BankOps,
}

impl AppOps {
    pub fn commit<C>(self, router: &mut App<C>)
    where
        C: Clone + fmt::Debug + PartialEq + JsonSchema,
    {
        self.bank.commit(&mut router.bank);
        self.wasm.commit(&mut router.wasm);
    }
}

impl<'a, C> AppCache<'a, C>
where
    C: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    fn new(router: &'a App<C>) -> Self {
        AppCache {
            router,
            wasm: router.wasm.cache(),
            bank: router.bank.cache(),
        }
    }

    /// When we want to commit the RouterCache, we need a 2 step process to satisfy Rust reference counting:
    /// 1. prepare() consumes RouterCache, releasing &Router, and creating a self-owned update info.
    /// 2. RouterOps::commit() can now take &mut Router and updates the underlying state
    pub fn prepare(self) -> AppOps {
        AppOps {
            wasm: self.wasm.prepare(),
            bank: self.bank.prepare(),
        }
    }

    /// This will execute the given messages, making all changes to the local cache.
    /// This *will* write some data to the cache if the message fails half-way through.
    /// All sequential calls to RouterCache will be one atomic unit (all commit or all fail).
    ///
    /// For normal use cases, you can use Router::execute() or Router::execute_multi().
    /// This is designed to be handled internally as part of larger process flows.
    fn execute(&mut self, sender: Addr, msg: SubMsg<C>) -> Result<AppResponse, String> {
        match msg.msg {
            CosmosMsg::Wasm(msg) => {
                let (resender, res) = self.handle_wasm(sender, msg)?;
                let mut attributes = res.attributes;
                // recurse in all messages
                for resend in res.messages {
                    let subres = self.execute(resender.clone(), SubMsg::new(resend))?;
                    // ignore the data now, just like in wasmd
                    // append the events
                    attributes.extend_from_slice(&subres.attributes);
                }
                Ok(AppResponse {
                    attributes,
                    data: res.data,
                })
            }
            CosmosMsg::Bank(msg) => {
                self.bank.execute(sender, msg)?;
                Ok(AppResponse::default())
            }
            _ => unimplemented!(),
        }
    }

    fn sudo(&mut self, contract_addr: Addr, msg: Vec<u8>) -> Result<AppResponse, String> {
        let res = self.wasm.sudo(contract_addr.clone(), self.router, msg)?;
        let mut attributes = res.attributes;
        // recurse in all messages
        for resend in res.messages {
            let subres = self.execute(contract_addr.clone(), resend)?;
            // ignore the data now, just like in wasmd
            // append the events
            attributes.extend_from_slice(&subres.attributes);
        }
        Ok(AppResponse {
            attributes,
            data: res.data,
        })
    }

    // this returns the contract address as well, so we can properly resend the data
    fn handle_wasm(
        &mut self,
        sender: Addr,
        msg: WasmMsg,
    ) -> Result<(Addr, ActionResponse<C>), String> {
        match msg {
            WasmMsg::Execute {
                contract_addr,
                msg,
                funds,
            } => {
                let contract_addr = Addr::unchecked(contract_addr);
                // first move the cash
                self.send(sender.clone(), contract_addr.clone().into(), &funds)?;
                // then call the contract
                let info = MessageInfo { sender, funds };
                let res =
                    self.wasm
                        .handle(contract_addr.clone(), self.router, info, msg.to_vec())?;
                Ok((contract_addr, res.into()))
            }
            WasmMsg::Instantiate {
                admin: _,
                code_id,
                msg,
                funds,
                label: _,
            } => {
                let contract_addr = Addr::unchecked(self.wasm.register_contract(code_id as usize)?);
                // move the cash
                self.send(sender.clone(), contract_addr.clone().into(), &funds)?;
                // then call the contract
                let info = MessageInfo { sender, funds };
                let res = self
                    .wasm
                    .init(contract_addr.clone(), self.router, info, msg.to_vec())?;
                Ok((
                    contract_addr.clone(),
                    ActionResponse::init(res, contract_addr),
                ))
            }
            WasmMsg::Migrate { .. } => unimplemented!(),
            m => panic!("Unsupported wasm message: {:?}", m),
        }
    }

    fn send<T: Into<Addr>>(
        &mut self,
        sender: T,
        recipient: String,
        amount: &[Coin],
    ) -> Result<AppResponse, String> {
        if !amount.is_empty() {
            let msg = BankMsg::Send {
                to_address: recipient,
                amount: amount.to_vec(),
            };
            self.bank.execute(sender.into(), msg)?;
        }
        Ok(AppResponse::default())
    }
}

// this parses the result from a wasm contract init
pub fn parse_contract_addr(data: &Option<Binary>) -> Result<Addr, String> {
    let bin = data
        .as_ref()
        .ok_or_else(|| "No data response".to_string())?
        .to_vec();
    let str = String::from_utf8(bin).map_err(|e| e.to_string())?;
    Ok(Addr::unchecked(str))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_helpers::{
        contract_payout, contract_payout_custom, contract_reflect, CustomMsg, EmptyMsg,
        PayoutMessage, ReflectMessage, ReflectResponse, ReflectSudoMsg,
    };
    use crate::SimpleBank;
    use cosmwasm_std::testing::MockStorage;
    use cosmwasm_std::{attr, coin, coins};

    fn mock_router() -> App {
        let env = mock_env();
        let api = Box::new(MockApi::default());
        let bank = SimpleBank {};

        App::new(api, env.block, bank, || Box::new(MockStorage::new()))
    }

    fn custom_router() -> App<CustomMsg> {
        let env = mock_env();
        let api = Box::new(MockApi::default());
        let bank = SimpleBank {};

        App::new(api, env.block, bank, || Box::new(MockStorage::new()))
    }

    fn get_balance<C>(router: &App<C>, addr: &Addr) -> Vec<Coin>
    where
        C: Clone + fmt::Debug + PartialEq + JsonSchema,
    {
        router.wrap().query_all_balances(addr).unwrap()
    }

    #[test]
    fn send_tokens() {
        let mut router = mock_router();

        let owner = Addr::unchecked("owner");
        let rcpt = Addr::unchecked("receiver");
        let init_funds = vec![coin(20, "btc"), coin(100, "eth")];
        let rcpt_funds = vec![coin(5, "btc")];

        // set money
        router.set_bank_balance(&owner, init_funds).unwrap();
        router.set_bank_balance(&rcpt, rcpt_funds).unwrap();

        // send both tokens
        let to_send = vec![coin(30, "eth"), coin(5, "btc")];
        let msg = SubMsg::new(BankMsg::Send {
            to_address: rcpt.clone().into(),
            amount: to_send,
        });
        router.execute(owner.clone(), msg.clone()).unwrap();
        let rich = get_balance(&router, &owner);
        assert_eq!(vec![coin(15, "btc"), coin(70, "eth")], rich);
        let poor = get_balance(&router, &rcpt);
        assert_eq!(vec![coin(10, "btc"), coin(30, "eth")], poor);

        // can send from other account (but funds will be deducted from sender)
        router.execute(rcpt.clone(), msg).unwrap();

        // cannot send too much
        let msg = SubMsg::new(BankMsg::Send {
            to_address: rcpt.into(),
            amount: coins(20, "btc"),
        });
        router.execute(owner.clone(), msg).unwrap_err();

        let rich = get_balance(&router, &owner);
        assert_eq!(vec![coin(15, "btc"), coin(70, "eth")], rich);
    }

    #[test]
    fn simple_contract() {
        let mut router = mock_router();

        // set personal balance
        let owner = Addr::unchecked("owner");
        let init_funds = vec![coin(20, "btc"), coin(100, "eth")];
        router.set_bank_balance(&owner, init_funds).unwrap();

        // set up contract
        let code_id = router.store_code(contract_payout());
        let msg = PayoutMessage {
            payout: coin(5, "eth"),
        };
        let contract_addr = router
            .instantiate_contract(code_id, owner.clone(), &msg, &coins(23, "eth"), "Payout")
            .unwrap();

        // sender funds deducted
        let sender = get_balance(&router, &owner);
        assert_eq!(sender, vec![coin(20, "btc"), coin(77, "eth")]);
        // get contract address, has funds
        let funds = get_balance(&router, &contract_addr);
        assert_eq!(funds, coins(23, "eth"));

        // create empty account
        let random = Addr::unchecked("random");
        let funds = get_balance(&router, &random);
        assert_eq!(funds, vec![]);

        // do one payout and see money coming in
        let res = router
            .execute_contract(random.clone(), contract_addr.clone(), &EmptyMsg {}, &[])
            .unwrap();
        assert_eq!(1, res.attributes.len());
        assert_eq!(&attr("action", "payout"), &res.attributes[0]);

        // random got cash
        let funds = get_balance(&router, &random);
        assert_eq!(funds, coins(5, "eth"));
        // contract lost it
        let funds = get_balance(&router, &contract_addr);
        assert_eq!(funds, coins(18, "eth"));
    }

    #[test]
    fn reflect_success() {
        let mut router = custom_router();

        // set personal balance
        let owner = Addr::unchecked("owner");
        let init_funds = vec![coin(20, "btc"), coin(100, "eth")];
        router.set_bank_balance(&owner, init_funds).unwrap();

        // set up payout contract
        let payout_id = router.store_code(contract_payout_custom());
        let msg = PayoutMessage {
            payout: coin(5, "eth"),
        };
        let payout_addr = router
            .instantiate_contract(payout_id, owner.clone(), &msg, &coins(23, "eth"), "Payout")
            .unwrap();

        // set up reflect contract
        let reflect_id = router.store_code(contract_reflect());
        let reflect_addr = router
            .instantiate_contract(reflect_id, owner, &EmptyMsg {}, &[], "Reflect")
            .unwrap();

        // reflect account is empty
        let funds = get_balance(&router, &reflect_addr);
        assert_eq!(funds, vec![]);
        // reflect count is 1
        let qres: ReflectResponse = router
            .wrap()
            .query_wasm_smart(&reflect_addr, &EmptyMsg {})
            .unwrap();
        assert_eq!(1, qres.count);

        // reflecting payout message pays reflect contract
        let msg = SubMsg::new(WasmMsg::Execute {
            contract_addr: payout_addr.into(),
            msg: b"{}".into(),
            funds: vec![],
        });
        let msgs = ReflectMessage {
            messages: vec![msg],
        };
        let res = router
            .execute_contract(Addr::unchecked("random"), reflect_addr.clone(), &msgs, &[])
            .unwrap();

        // ensure the attributes were relayed from the sub-message
        assert_eq!(1, res.attributes.len());
        assert_eq!(&attr("action", "payout"), &res.attributes[0]);

        // ensure transfer was executed with reflect as sender
        let funds = get_balance(&router, &reflect_addr);
        assert_eq!(funds, coins(5, "eth"));

        // reflect count updated
        let qres: ReflectResponse = router
            .wrap()
            .query_wasm_smart(&reflect_addr, &EmptyMsg {})
            .unwrap();
        assert_eq!(2, qres.count);
    }

    #[test]
    fn reflect_error() {
        let mut router = custom_router();

        // set personal balance
        let owner = Addr::unchecked("owner");
        let init_funds = vec![coin(20, "btc"), coin(100, "eth")];
        router.set_bank_balance(&owner, init_funds).unwrap();

        // set up reflect contract
        let reflect_id = router.store_code(contract_reflect());
        let reflect_addr = router
            .instantiate_contract(
                reflect_id,
                owner,
                &EmptyMsg {},
                &coins(40, "eth"),
                "Reflect",
            )
            .unwrap();

        // reflect has 40 eth
        let funds = get_balance(&router, &reflect_addr);
        assert_eq!(funds, coins(40, "eth"));
        let random = Addr::unchecked("random");

        // sending 7 eth works
        let msg = SubMsg::new(BankMsg::Send {
            to_address: random.clone().into(),
            amount: coins(7, "eth"),
        });
        let msgs = ReflectMessage {
            messages: vec![msg],
        };
        let res = router
            .execute_contract(random.clone(), reflect_addr.clone(), &msgs, &[])
            .unwrap();
        assert_eq!(0, res.attributes.len());
        // ensure random got paid
        let funds = get_balance(&router, &random);
        assert_eq!(funds, coins(7, "eth"));

        // reflect count should be updated to 2
        let qres: ReflectResponse = router
            .wrap()
            .query_wasm_smart(&reflect_addr, &EmptyMsg {})
            .unwrap();
        assert_eq!(2, qres.count);

        // sending 8 eth, then 3 btc should fail both
        let msg = SubMsg::new(BankMsg::Send {
            to_address: random.clone().into(),
            amount: coins(8, "eth"),
        });
        let msg2 = SubMsg::new(BankMsg::Send {
            to_address: random.clone().into(),
            amount: coins(3, "btc"),
        });
        let msgs = ReflectMessage {
            messages: vec![msg, msg2],
        };
        let err = router
            .execute_contract(random.clone(), reflect_addr.clone(), &msgs, &[])
            .unwrap_err();
        assert_eq!("Overflow: Cannot Sub with 0 and 3", err.as_str());

        // first one should have been rolled-back on error (no second payment)
        let funds = get_balance(&router, &random);
        assert_eq!(funds, coins(7, "eth"));

        // failure should not update reflect count
        let qres: ReflectResponse = router
            .wrap()
            .query_wasm_smart(&reflect_addr, &EmptyMsg {})
            .unwrap();
        assert_eq!(2, qres.count);
    }

    #[test]
    fn sudo_works() {
        let mut router = custom_router();

        let owner = Addr::unchecked("owner");
        let reflect_id = router.store_code(contract_reflect());
        let reflect_addr = router
            .instantiate_contract(reflect_id, owner, &EmptyMsg {}, &[], "Reflect")
            .unwrap();

        // count is 1
        let ReflectResponse { count } = router
            .wrap()
            .query_wasm_smart(&reflect_addr, &EmptyMsg {})
            .unwrap();
        assert_eq!(1, count);

        // sudo
        let msg = ReflectSudoMsg { set_count: 25 };
        router.sudo(reflect_addr.clone(), &msg).unwrap();

        // count is 25
        let ReflectResponse { count } = router
            .wrap()
            .query_wasm_smart(&reflect_addr, &EmptyMsg {})
            .unwrap();
        assert_eq!(25, count);
    }
}
