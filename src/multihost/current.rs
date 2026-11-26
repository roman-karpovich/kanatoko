use super::Session;
use crate::soroban_sdk::{
    testutils::Ledger,
    xdr::{self, FromXdr, ToXdr},
    Bytes, Val, Vec as Args,
};
use crate::{canonical_ledger_digest, ScenarioFork};
impl Session for ScenarioFork<'_> {
    fn call(&self, contract: &str, function: &str, args: &[u8]) -> Result<Vec<u8>, String> {
        let env = self.env();
        let args = Args::<Val>::from_xdr(env, &Bytes::from_slice(env, args))
            .map_err(|_| "invalid XDR argument vector".to_owned())?;
        self.try_invoke::<Val>(&self.contract(contract), function, args)
            .map(|v| v.to_xdr(env).iter().collect())
            .map_err(|failure| format!("{:?}", failure.kind))
    }
    fn mock_funded_source_account(&self, label: &str, stroops: i64) -> String {
        self.mock_all_auths();
        let account = self.local_account(label);
        self.fund_local_account(&account, stroops);
        let xdr::ScAddress::Account(id) = xdr::ScAddress::from(&account) else {
            unreachable!()
        };
        self.env().host().set_source_account(id).unwrap();
        account.to_string().to_string()
    }
    fn protocol_version(&self) -> u32 {
        self.env()
            .host()
            .with_ledger_info(|ledger| Ok(ledger.protocol_version))
            .unwrap()
    }
    fn timestamp(&self) -> u64 {
        self.env().ledger().timestamp()
    }
    fn set_timestamp(&self, t: u64) {
        self.env().ledger().set_timestamp(t);
    }
    fn replace_wasm(&self, contract: &str, wasm: &[u8]) -> [u8; 32] {
        self.replace_wasm(&self.contract(contract), wasm)
    }
    fn digest(&self) -> [u8; 32] {
        canonical_ledger_digest(&self.env().to_ledger_snapshot()).unwrap()
    }
    fn storage_digest(&self, replaced_contract: &str) -> [u8; 32] {
        let address = xdr::ScAddress::from(&self.contract(replaced_contract));
        super::storage_digest(self.env().to_ledger_snapshot(), &address)
    }
}
