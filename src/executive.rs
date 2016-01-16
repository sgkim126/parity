//! Transaction Execution environment.
use common::*;
use state::*;
use engine::*;
use evm::{self, Ext};
use externalities::*;
use substate::*;

/// Returns new address created from address and given nonce.
pub fn contract_address(address: &Address, nonce: &U256) -> Address {
	let mut stream = RlpStream::new_list(2);
	stream.append(address);
	stream.append(nonce);
	From::from(stream.out().sha3())
}

/// Transaction execution receipt.
#[derive(Debug)]
pub struct Executed {
	/// Gas paid up front for execution of transaction.
	pub gas: U256,
	/// Gas used during execution of transaction.
	pub gas_used: U256,
	/// Gas refunded after the execution of transaction. 
	/// To get gas that was required up front, add `refunded` and `gas_used`.
	pub refunded: U256,
	/// Cumulative gas used in current block so far.
	/// 
	/// `cumulative_gas_used = gas_used(t0) + gas_used(t1) + ... gas_used(tn)`
	///
	/// where `tn` is current transaction.
	pub cumulative_gas_used: U256,
	/// Vector of logs generated by transaction.
	pub logs: Vec<LogEntry>,
	/// Addresses of contracts created during execution of transaction.
	/// Ordered from earliest creation.
	/// 
	/// eg. sender creates contract A and A in constructor creates contract B 
	/// 
	/// B creation ends first, and it will be the first element of the vector.
	pub contracts_created: Vec<Address>
}

/// Transaction execution result.
pub type ExecutionResult = Result<Executed, ExecutionError>;

/// Transaction executor.
pub struct Executive<'a> {
	state: &'a mut State,
	info: &'a EnvInfo,
	engine: &'a Engine,
	depth: usize
}

impl<'a> Executive<'a> {
	/// Basic constructor.
	pub fn new(state: &'a mut State, info: &'a EnvInfo, engine: &'a Engine) -> Self {
		Executive::new_with_depth(state, info, engine, 0)
	}

	/// Populates executive from parent properties. Increments executive depth.
	pub fn from_parent(state: &'a mut State, info: &'a EnvInfo, engine: &'a Engine, depth: usize) -> Self {
		Executive::new_with_depth(state, info, engine, depth + 1)
	}

	/// Helper constructor. Should be used to create `Executive` with desired depth.
	/// Private.
	fn new_with_depth(state: &'a mut State, info: &'a EnvInfo, engine: &'a Engine, depth: usize) -> Self {
		Executive {
			state: state,
			info: info,
			engine: engine,
			depth: depth
		}
	}

	/// Creates `Externalities` from `Executive`.
	pub fn to_externalities<'_>(&'_ mut self, params: &'_ ActionParams, substate: &'_ mut Substate, output: OutputPolicy<'_>) -> Externalities {
		Externalities::new(self.state, self.info, self.engine, self.depth, params, substate, output)
	}

	/// This funtion should be used to execute transaction.
	pub fn transact(&'a mut self, t: &Transaction) -> Result<Executed, Error> {
		let sender = try!(t.sender());
		let nonce = self.state.nonce(&sender);

		let schedule = self.engine.schedule(self.info);
		let base_gas_required = U256::from(t.gas_required(&schedule));

		if t.gas < base_gas_required {
			return Err(From::from(ExecutionError::NotEnoughBaseGas { required: base_gas_required, got: t.gas }));
		}

		let init_gas = t.gas - base_gas_required;

		// validate transaction nonce
		if t.nonce != nonce {
			return Err(From::from(ExecutionError::InvalidNonce { expected: nonce, got: t.nonce }));
		}
		
		// validate if transaction fits into given block
		if self.info.gas_used + t.gas > self.info.gas_limit {
			return Err(From::from(ExecutionError::BlockGasLimitReached { 
				gas_limit: self.info.gas_limit, 
				gas_used: self.info.gas_used, 
				gas: t.gas 
			}));
		}

		// TODO: we might need bigints here, or at least check overflows.
		let balance = self.state.balance(&sender);
		let gas_cost = U512::from(t.gas) * U512::from(t.gas_price);
		let total_cost = U512::from(t.value) + gas_cost;

		// avoid unaffordable transactions
		if U512::from(balance) < total_cost {
			return Err(From::from(ExecutionError::NotEnoughCash { required: total_cost, got: U512::from(balance) }));
		}

		// NOTE: there can be no invalid transactions from this point.
		self.state.inc_nonce(&sender);
		self.state.sub_balance(&sender, &U256::from(gas_cost));

		let mut substate = Substate::new();

		let res = match t.action() {
			&Action::Create => {
				let new_address = contract_address(&sender, &nonce);
				let params = ActionParams {
					code_address: new_address.clone(),
					address: new_address,
					sender: sender.clone(),
					origin: sender.clone(),
					gas: init_gas,
					gas_price: t.gas_price,
					value: t.value,
					code: Some(t.data.clone()),
					data: None,
				};
				self.create(&params, &mut substate)
			},
			&Action::Call(ref address) => {
				let params = ActionParams {
					code_address: address.clone(),
					address: address.clone(),
					sender: sender.clone(),
					origin: sender.clone(),
					gas: init_gas,
					gas_price: t.gas_price,
					value: t.value,
					code: self.state.code(address),
					data: Some(t.data.clone()),
				};
				// TODO: move output upstream
				let mut out = vec![];
				self.call(&params, &mut substate, BytesRef::Flexible(&mut out))
			}
		};

		// finalize here!
		Ok(try!(self.finalize(t, substate, res)))
	}

	/// Calls contract function with given contract params.
	/// NOTE. It does not finalize the transaction (doesn't do refunds, nor suicides).
	/// Modifies the substate and the output.
	/// Returns either gas_left or `evm::Error`.
	pub fn call(&mut self, params: &ActionParams, substate: &mut Substate, mut output: BytesRef) -> evm::Result {
		// backup used in case of running out of gas
		let backup = self.state.clone();

		// at first, transfer value to destination
		self.state.transfer_balance(&params.sender, &params.address, &params.value);

		if self.engine.is_builtin(&params.code_address) {
			// if destination is builtin, try to execute it
			
			let default = [];
			let data = if let &Some(ref d) = &params.data { d as &[u8] } else { &default as &[u8] };

			let cost = self.engine.cost_of_builtin(&params.code_address, data);
			match cost <= params.gas {
				true => {
					self.engine.execute_builtin(&params.code_address, data, &mut output);
					Ok(params.gas - cost)
				},
				// just drain the whole gas
				false => {
					self.state.revert(backup);
					Err(evm::Error::OutOfGas)
				}
			}
		} else if params.code.is_some() {
			// if destination is a contract, do normal message call
			
			// part of substate that may be reverted
			let mut unconfirmed_substate = Substate::new();

			let res = {
				let mut ext = self.to_externalities(params, &mut unconfirmed_substate, OutputPolicy::Return(output));
				self.engine.vm_factory().create().exec(&params, &mut ext)
			};
			self.enact_result(&res, substate, unconfirmed_substate, backup);
			res
		} else {
			// otherwise, nothing
			Ok(params.gas)
		}
	}
	
	/// Creates contract with given contract params.
	/// NOTE. It does not finalize the transaction (doesn't do refunds, nor suicides).
	/// Modifies the substate.
	pub fn create(&mut self, params: &ActionParams, substate: &mut Substate) -> evm::Result {
		// backup used in case of running out of gas
		let backup = self.state.clone();

		// part of substate that may be reverted
		let mut unconfirmed_substate = Substate::new();

		// at first create new contract
		self.state.new_contract(&params.address);

		// then transfer value to it
		self.state.transfer_balance(&params.sender, &params.address, &params.value);

		let res = {
			let mut ext = self.to_externalities(params, &mut unconfirmed_substate, OutputPolicy::InitContract);
			self.engine.vm_factory().create().exec(&params, &mut ext)
		};
		self.enact_result(&res, substate, unconfirmed_substate, backup);
		res
	}

	/// Finalizes the transaction (does refunds and suicides).
	fn finalize(&mut self, t: &Transaction, substate: Substate, result: evm::Result) -> ExecutionResult {
		let schedule = self.engine.schedule(self.info);

		// refunds from SSTORE nonzero -> zero
		let sstore_refunds = U256::from(schedule.sstore_refund_gas) * substate.refunds_count;
		// refunds from contract suicides
		let suicide_refunds = U256::from(schedule.suicide_refund_gas) * U256::from(substate.suicides.len());

		// real ammount to refund
		let gas_left = match &result { &Ok(x) => x, _ => x!(0) };
		let refund = cmp::min(sstore_refunds + suicide_refunds, (t.gas - gas_left) / U256::from(2)) + gas_left;
		let refund_value = refund * t.gas_price;
		trace!("Refunding sender: sstore0s: {}, suicides: {}, gas_left: {}, refund: {}, refund_value: {}, sender: {}", sstore_refunds, suicide_refunds, gas_left, refund, refund_value, t.sender().unwrap());
		self.state.add_balance(&t.sender().unwrap(), &refund_value);
		
		// fees earned by author
		let fees = t.gas - refund;
		let fees_value = fees * t.gas_price;
		let author = &self.info.author;
		self.state.add_balance(author, &fees_value);
		trace!("Compensating author: fees: {}, fees_value: {}, author: {}", fees, fees_value, author);

		// perform suicides
		for address in substate.suicides.iter() {
			trace!("Killing {}", address);
			self.state.kill_account(address);
		}

		let gas_used = t.gas - gas_left;

		match result { 
			Err(evm::Error::Internal) => Err(ExecutionError::Internal),
			// TODO [ToDr] BadJumpDestination @debris - how to handle that?
			Err(evm::Error::OutOfGas) 
				| Err(evm::Error::BadJumpDestination { destination: _ }) 
				| Err(evm::Error::BadInstruction { instruction: _ }) 
				| Err(evm::Error::StackUnderflow {instruction: _, wanted: _, on_stack: _})
				| Err(evm::Error::OutOfStack {instruction: _, wanted: _, limit: _}) => {
				Ok(Executed {
					gas: t.gas,
					gas_used: t.gas,
					refunded: U256::zero(),
					cumulative_gas_used: self.info.gas_used + t.gas,
					logs: vec![],
					contracts_created: vec![]
				})
			},
			Ok(_) => {
				Ok(Executed {
					gas: t.gas,
					gas_used: gas_used,
					refunded: refund,
					cumulative_gas_used: self.info.gas_used + gas_used,
					logs: substate.logs,
					contracts_created: substate.contracts_created
				})
			}
		}
	}

	fn enact_result(&mut self, result: &evm::Result, substate: &mut Substate, un_substate: Substate, backup: State) {
		// TODO: handle other evm::Errors same as OutOfGas once they are implemented
		match result {
			&Err(evm::Error::OutOfGas)
				| &Err(evm::Error::BadJumpDestination { destination: _ }) 
				| &Err(evm::Error::BadInstruction { instruction: _ }) 
				| &Err(evm::Error::StackUnderflow {instruction: _, wanted: _, on_stack: _})
				| &Err(evm::Error::OutOfStack {instruction: _, wanted: _, limit: _}) => {
				self.state.revert(backup);
			},
			&Ok(_) | &Err(evm::Error::Internal) => substate.accrue(un_substate)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use common::*;
	use state::*;
	use ethereum;
	use engine::*;
	use spec::*;
	use evm::{Schedule, Factory, VMType};
	use substate::*;

	struct TestEngine {
		factory: Factory,
		spec: Spec,
		max_depth: usize
	}

	impl TestEngine {
		fn new(max_depth: usize, factory: Factory) -> TestEngine {
			TestEngine {
				factory: factory,
				spec: ethereum::new_frontier_test(),
				max_depth: max_depth 
			}
		}
	}

	impl Engine for TestEngine {
		fn name(&self) -> &str { "TestEngine" }
		fn spec(&self) -> &Spec { &self.spec }
		fn vm_factory(&self) -> &Factory {
			&self.factory
		}
		fn schedule(&self, _env_info: &EnvInfo) -> Schedule { 
			let mut schedule = Schedule::new_frontier();
			schedule.max_depth = self.max_depth;
			schedule
		}
	}

	#[test]
	fn test_contract_address() {
		let address = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let expected_address = Address::from_str("3f09c73a5ed19289fb9bdc72f1742566df146f56").unwrap();
		assert_eq!(expected_address, contract_address(&address, &U256::from(88)));
	}

	// TODO: replace params with transactions!
	evm_test!{test_sender_balance: test_sender_balance_jit, test_sender_balance_int}
	fn test_sender_balance(factory: Factory) {
		let sender = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let address = contract_address(&sender, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some("3331600055".from_hex().unwrap());
		params.value = U256::from(0x7);
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(0x100u64));
		let info = EnvInfo::new();
		let engine = TestEngine::new(0, factory);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate).unwrap()
		};

		assert_eq!(gas_left, U256::from(79_975));
		assert_eq!(state.storage_at(&address, &H256::new()), H256::from(&U256::from(0xf9u64)));
		assert_eq!(state.balance(&sender), U256::from(0xf9));
		assert_eq!(state.balance(&address), U256::from(0x7));
		// 0 cause contract hasn't returned
		assert_eq!(substate.contracts_created.len(), 0);

		// TODO: just test state root.
	}

	evm_test!{test_create_contract: test_create_contract_jit, test_create_contract_int}
	fn test_create_contract(factory: Factory) {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 17 - push 17
		// f0 - create
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d60036017f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(&sender, &U256::zero());
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(code.clone());
		params.value = U256::from(100);
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(100));
		let info = EnvInfo::new();
		let engine = TestEngine::new(0, factory);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate).unwrap()
		};
		
		assert_eq!(gas_left, U256::from(62_976));
		// ended with max depth
		assert_eq!(substate.contracts_created.len(), 0);
	}

	evm_test!{test_create_contract_value_too_high: test_create_contract_value_too_high_jit, test_create_contract_value_too_high_int}
	fn test_create_contract_value_too_high(factory: Factory) {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 e6 - push 230
		// f0 - create a contract trying to send 230.
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d600360e6f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(&sender, &U256::zero());
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(code.clone());
		params.value = U256::from(100);
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(100));
		let info = EnvInfo::new();
		let engine = TestEngine::new(0, factory);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate).unwrap()
		};
		
		assert_eq!(gas_left, U256::from(62_976));
		assert_eq!(substate.contracts_created.len(), 0);
	}

	evm_test!{test_create_contract_without_max_depth: test_create_contract_without_max_depth_jit, test_create_contract_without_max_depth_int}
	fn test_create_contract_without_max_depth(factory: Factory) {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 17 - push 17
		// f0 - create
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d60036017f0".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(&sender, &U256::zero());
		let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(code.clone());
		params.value = U256::from(100);
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(100));
		let info = EnvInfo::new();
		let engine = TestEngine::new(1024, factory);
		let mut substate = Substate::new();

		{
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate).unwrap();
		}
		
		assert_eq!(substate.contracts_created.len(), 1);
		assert_eq!(substate.contracts_created[0], next_address);
	}

	evm_test!{test_aba_calls: test_aba_calls_jit, test_aba_calls_int}
	fn test_aba_calls(factory: Factory) {
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 18 - push 18
		// 73 945304eb96065b2a98b57a48a06ae28d285a71b5 - push this address
		// 61 03e8 - push 1000
		// f1 - message call
		// 58 - get PC
		// 55 - sstore

		let code_a = "6000600060006000601873945304eb96065b2a98b57a48a06ae28d285a71b56103e8f15855".from_hex().unwrap();

		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 17 - push 17
		// 73 0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6 - push this address
		// 61 0x01f4 - push 500
		// f1 - message call
		// 60 01 - push 1
		// 01 - add
		// 58 - get PC
		// 55 - sstore
		let code_b = "60006000600060006017730f572e5295c57f15886f9b263e2f6d2d6c7b5ec66101f4f16001015855".from_hex().unwrap();

		let address_a = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let address_b = Address::from_str("945304eb96065b2a98b57a48a06ae28d285a71b5" ).unwrap();
		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();

		let mut params = ActionParams::new();
		params.address = address_a.clone();
		params.sender = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(code_a.clone());
		params.value = U256::from(100_000);

		let mut state = State::new_temp();
		state.init_code(&address_a, code_a.clone());
		state.init_code(&address_b, code_b.clone());
		state.add_balance(&sender, &U256::from(100_000));

		let info = EnvInfo::new();
		let engine = TestEngine::new(0, factory);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.call(&params, &mut substate, BytesRef::Fixed(&mut [])).unwrap()
		};

		assert_eq!(gas_left, U256::from(73_237));
		assert_eq!(state.storage_at(&address_a, &H256::from(&U256::from(0x23))), H256::from(&U256::from(1)));
	}

	evm_test!{test_recursive_bomb1: test_recursive_bomb1_jit, test_recursive_bomb1_int}
	fn test_recursive_bomb1(factory: Factory) {
		// 60 01 - push 1
		// 60 00 - push 0
		// 54 - sload 
		// 01 - add
		// 60 00 - push 0
		// 55 - sstore
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 30 - load address
		// 60 e0 - push e0
		// 5a - get gas
		// 03 - sub
		// f1 - message call (self in this case)
		// 60 01 - push 1
		// 55 - sstore
		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let code = "600160005401600055600060006000600060003060e05a03f1600155".from_hex().unwrap();
		let address = contract_address(&sender, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.gas = U256::from(100_000);
		params.code = Some(code.clone());
		let mut state = State::new_temp();
		state.init_code(&address, code.clone());
		let info = EnvInfo::new();
		let engine = TestEngine::new(0, factory);
		let mut substate = Substate::new();

		let gas_left = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.call(&params, &mut substate, BytesRef::Fixed(&mut [])).unwrap()
		};

		assert_eq!(gas_left, U256::from(59_870));
		assert_eq!(state.storage_at(&address, &H256::from(&U256::zero())), H256::from(&U256::from(1)));
		assert_eq!(state.storage_at(&address, &H256::from(&U256::one())), H256::from(&U256::from(1)));
	}

	evm_test!{test_transact_simple: test_transact_simple_jit, test_transact_simple_int}
	fn test_transact_simple(factory: Factory) {
		let mut t = Transaction::new_create(U256::from(17), "3331600055".from_hex().unwrap(), U256::from(100_000), U256::zero(), U256::zero());
		let keypair = KeyPair::create().unwrap();
		t.sign(&keypair.secret());
		let sender = t.sender().unwrap();
		let contract = contract_address(&sender, &U256::zero());

		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(18));	
		let mut info = EnvInfo::new();
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0, factory);

		let executed = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t).unwrap()
		};

		assert_eq!(executed.gas, U256::from(100_000));
		assert_eq!(executed.gas_used, U256::from(41_301));
		assert_eq!(executed.refunded, U256::from(58_699));
		assert_eq!(executed.cumulative_gas_used, U256::from(41_301));
		assert_eq!(executed.logs.len(), 0);
		assert_eq!(executed.contracts_created.len(), 0);
		assert_eq!(state.balance(&sender), U256::from(1));
		assert_eq!(state.balance(&contract), U256::from(17));
		assert_eq!(state.nonce(&sender), U256::from(1));
		assert_eq!(state.storage_at(&contract, &H256::new()), H256::from(&U256::from(1)));
	}

	evm_test!{test_transact_invalid_sender: test_transact_invalid_sender_jit, test_transact_invalid_sender_int}
	fn test_transact_invalid_sender(factory: Factory) {
		let t = Transaction::new_create(U256::from(17), "3331600055".from_hex().unwrap(), U256::from(100_000), U256::zero(), U256::zero());

		let mut state = State::new_temp();
		let mut info = EnvInfo::new();
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0, factory);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t)
		};
		
		match res {
			Err(Error::Util(UtilError::Crypto(CryptoError::InvalidSignature))) => (),
			_ => assert!(false, "Expected invalid signature error.")
		}
	}

	evm_test!{test_transact_invalid_nonce: test_transact_invalid_nonce_jit, test_transact_invalid_nonce_int}
	fn test_transact_invalid_nonce(factory: Factory) {
		let mut t = Transaction::new_create(U256::from(17), "3331600055".from_hex().unwrap(), U256::from(100_000), U256::zero(), U256::one());
		let keypair = KeyPair::create().unwrap();
		t.sign(&keypair.secret());
		let sender = t.sender().unwrap();
		
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(17));	
		let mut info = EnvInfo::new();
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0, factory);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t)
		};
		
		match res {
			Err(Error::Execution(ExecutionError::InvalidNonce { expected, got })) 
				if expected == U256::zero() && got == U256::one() => (), 
			_ => assert!(false, "Expected invalid nonce error.")
		}
	}

	evm_test!{test_transact_gas_limit_reached: test_transact_gas_limit_reached_jit, test_transact_gas_limit_reached_int}
	fn test_transact_gas_limit_reached(factory: Factory) {
		let mut t = Transaction::new_create(U256::from(17), "3331600055".from_hex().unwrap(), U256::from(80_001), U256::zero(), U256::zero());
		let keypair = KeyPair::create().unwrap();
		t.sign(&keypair.secret());
		let sender = t.sender().unwrap();

		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(17));	
		let mut info = EnvInfo::new();
		info.gas_used = U256::from(20_000);
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0, factory);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t)
		};

		match res {
			Err(Error::Execution(ExecutionError::BlockGasLimitReached { gas_limit, gas_used, gas })) 
				if gas_limit == U256::from(100_000) && gas_used == U256::from(20_000) && gas == U256::from(80_001) => (), 
			_ => assert!(false, "Expected block gas limit error.")
		}
	}

	evm_test!{test_not_enough_cash: test_not_enough_cash_jit, test_not_enough_cash_int}
	fn test_not_enough_cash(factory: Factory) {
		let mut t = Transaction::new_create(U256::from(18), "3331600055".from_hex().unwrap(), U256::from(100_000), U256::one(), U256::zero());
		let keypair = KeyPair::create().unwrap();
		t.sign(&keypair.secret());
		let sender = t.sender().unwrap();

		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from(100_017));	
		let mut info = EnvInfo::new();
		info.gas_limit = U256::from(100_000);
		let engine = TestEngine::new(0, factory);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.transact(&t)
		};
		
		match res {
			Err(Error::Execution(ExecutionError::NotEnoughCash { required , got })) 
				if required == U512::from(100_018) && got == U512::from(100_017) => (), 
			_ => assert!(false, "Expected not enough cash error. {:?}", res)
		}
	}

	evm_test!{test_sha3: test_sha3_jit, test_sha3_int}
	fn test_sha3(factory: Factory) {
		let code = "6064640fffffffff20600055".from_hex().unwrap();

		let sender = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let address = contract_address(&sender, &U256::zero());
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::new();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(0x0186a0);
		params.code = code.clone();
		params.value = U256::from_str("0de0b6b3a7640000").unwrap();
		let mut state = State::new_temp();
		state.add_balance(&sender, &U256::from_str("152d02c7e14af6800000").unwrap());
		let info = EnvInfo::new();
		let engine = TestEngine::new(0, factory);
		let mut substate = Substate::new();

		let result = {
			let mut ex = Executive::new(&mut state, &info, &engine);
			ex.create(&params, &mut substate)
		};

		match result {
			Err(_) => {
			},
			_ => {
				panic!("Expected OutOfGas");
			}
		}
	}

}
