//! Inheritance vault (v1: NEAR + Ethereum). One contract, many vaults.
//!
//! The contract controls an Ethereum address through NEAR Chain Signatures.
//! The owner checks in; after a silence period, guardians (m-of-n) can start
//! a countdown the owner can still cancel; when it ends, anyone can ask the
//! contract to sign payouts to the heirs the owner named in advance.
//!
//! Guardians only decide WHEN. They can never change WHO gets paid.

use near_sdk::json_types::U128;
use near_sdk::serde_json::{json, Value};
use near_sdk::store::LookupMap;
use near_sdk::{
    env, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseError,
};

const NS_PER_SEC: u128 = 1_000_000_000;
/// Longest allowed timer: 100 years. Keeps nanosecond math inside u64.
const MAX_SPAN_NS: u128 = 100 * 365 * 86_400 * NS_PER_SEC;
/// Gas limit for one payout. A transfer to an address already in use needs
/// 21,000, but a brand-new empty address needed about 207,000 on Sepolia.
/// Unused gas is not charged, so what is not needed stays in the vault.
const TRANSFER_GAS_LIMIT: u64 = 300_000;
const MAX_HEIRS: usize = 10;
const MAX_GUARDIANS: usize = 7;
const MAX_LABEL: usize = 64;
const SIGN_GAS: Gas = Gas::from_tgas(250);
const CALLBACK_GAS: Gas = Gas::from_tgas(15);

// ---------------------------------------------------------------- time spans

#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unit {
    Minutes,
    Hours,
    Days,
    Months,
    Years,
}

impl Unit {
    /// Months are 30 days and years are 365 days. Stated plainly so the rules
    /// anyone reads on-chain are unambiguous.
    fn seconds(self) -> u128 {
        match self {
            Unit::Minutes => 60,
            Unit::Hours => 3_600,
            Unit::Days => 86_400,
            Unit::Months => 30 * 86_400,
            Unit::Years => 365 * 86_400,
        }
    }
}

/// What the owner types in the form: a number and a unit.
#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug)]
pub struct TimeSpan {
    pub amount: u32,
    pub unit: Unit,
}

impl TimeSpan {
    pub fn to_ns(&self) -> u64 {
        let ns = self.amount as u128 * self.unit.seconds() * NS_PER_SEC;
        require!(ns > 0, "a timer must be longer than zero");
        require!(ns <= MAX_SPAN_NS, "a timer can be at most 100 years");
        ns as u64
    }
}

// --------------------------------------------------------------------- heirs

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct Heir {
    /// 0x-prefixed Ethereum address.
    pub eth_address: String,
    /// Share in basis points (10000 = 100%).
    pub bps: u16,
    /// Random secret for hidden-heir mode. Empty for public heirs.
    pub salt: String,
    /// A name for this address, for example "Anna". Public heirs only. Never
    /// used for payouts and not part of the hidden-heir commitment. Visible to
    /// anyone reading the contract, so the owner may prefer to keep names in
    /// the app instead. The owner can change it at any time.
    #[serde(default)]
    pub label: String,
}

fn normalize_address(a: &str) -> String {
    let s = a.to_lowercase();
    require!(
        s.len() == 42 && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit()),
        "invalid Ethereum address"
    );
    s
}

fn address_bytes(a: &str) -> Vec<u8> {
    hex::decode(&a[2..]).expect("validated address")
}

fn commitment(h: &Heir) -> String {
    let preimage = format!("{}|{}|{}", h.eth_address.to_lowercase(), h.bps, h.salt);
    hex::encode(env::sha256(preimage.as_bytes()))
}

fn check_name(name: &str) {
    require!(!name.trim().is_empty() && name.len() <= 64, "name must be 1 to 64 characters");
}

/// Guardians are optional. If there are any, the threshold must fit them.
fn check_guardians(guardians: &[AccountId], threshold: u8, owner: &AccountId) {
    require!(guardians.len() <= MAX_GUARDIANS, "at most 7 guardians");
    if !guardians.is_empty() {
        require!(
            threshold >= 1 && (threshold as usize) <= guardians.len(),
            "threshold must be between 1 and the number of guardians"
        );
    }
    for (i, g) in guardians.iter().enumerate() {
        require!(g != owner, "the owner cannot be a guardian");
        require!(!guardians[..i].contains(g), "duplicate guardian");
    }
}

/// Heirs are optional. Public heirs OR hidden commitments, never both.
/// Returns (hidden, heirs, commitments).
fn resolve_heirs(public_heirs: Vec<Heir>, heir_commitments: Vec<String>) -> (bool, Vec<Heir>, Vec<String>) {
    match (public_heirs.is_empty(), heir_commitments.is_empty()) {
        (true, true) => (false, vec![], vec![]),
        (false, true) => (false, check_heirs(&public_heirs), vec![]),
        (true, false) => {
            require!(heir_commitments.len() <= MAX_HEIRS, "at most 10 heirs");
            for c in &heir_commitments {
                require!(c.len() == 64 && hex::decode(c).is_ok(), "commitments are 32-byte hex");
            }
            (true, vec![], heir_commitments)
        }
        _ => env::panic_str("give either public heirs or heir commitments, not both"),
    }
}

fn check_heirs(heirs: &[Heir]) -> Vec<Heir> {
    require!(!heirs.is_empty() && heirs.len() <= MAX_HEIRS, "1 to 10 heirs required");
    let mut total: u32 = 0;
    let mut out = Vec::new();
    for h in heirs {
        require!(h.bps > 0, "each heir needs a share above zero");
        require!(h.label.len() <= MAX_LABEL, "an heir name can be at most 64 characters");
        total += h.bps as u32;
        let mut c = h.clone();
        c.eth_address = normalize_address(&h.eth_address);
        require!(
            !out.iter().any(|x: &Heir| x.eth_address == c.eth_address),
            "duplicate heir address"
        );
        out.push(c);
    }
    require!(total == 10_000, "heir shares must add up to exactly 10000 bps (100%)");
    out
}

// ------------------------------------------------------------- Ethereum RLP

fn be_trim(n: u128) -> Vec<u8> {
    let b = n.to_be_bytes();
    let i = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    b[i..].to_vec()
}

fn len_prefix(len: usize, offset: u8) -> Vec<u8> {
    if len < 56 {
        vec![offset + len as u8]
    } else {
        let be = be_trim(len as u128);
        let mut v = vec![offset + 55 + be.len() as u8];
        v.extend(be);
        v
    }
}

fn rlp_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return vec![b[0]];
    }
    let mut out = len_prefix(b.len(), 0x80);
    out.extend_from_slice(b);
    out
}

fn rlp_uint(n: u128) -> Vec<u8> {
    rlp_bytes(&be_trim(n))
}

fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.concat();
    let mut out = len_prefix(payload.len(), 0xc0);
    out.extend(payload);
    out
}

/// Unsigned EIP-1559 ETH transfer (type 0x02), empty data and access list.
pub fn eip1559_unsigned(
    chain_id: u64,
    nonce: u64,
    max_priority_fee: u128,
    max_fee: u128,
    gas_limit: u64,
    to: &[u8],
    value: u128,
) -> Vec<u8> {
    let fields = vec![
        rlp_uint(chain_id as u128),
        rlp_uint(nonce as u128),
        rlp_uint(max_priority_fee),
        rlp_uint(max_fee),
        rlp_uint(gas_limit as u128),
        rlp_bytes(to),
        rlp_uint(value),
        rlp_bytes(&[]),
        rlp_list(&[]),
    ];
    let mut out = vec![0x02];
    out.extend(rlp_list(&fields));
    out
}

// -------------------------------------------------------------------- state

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct ExecutionPlan {
    pub base_nonce: u64,
    pub total_wei: U128,
    pub max_fee_per_gas: U128,
    pub max_priority_fee_per_gas: U128,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Release {
    pub approvals: Vec<AccountId>,
    pub started_at_ns: u64,
    /// Set when enough guardians have approved: now + waiting period.
    pub executable_at_ns: Option<u64>,
    pub plan: Option<ExecutionPlan>,
}

/// One vault: the rules and the live state of a single family (or workplace).
/// Every vault has its own Ethereum address, because the signing path
/// `vault-<id>` is different for each one.
#[near(serializers = [borsh])]
pub struct VaultData {
    owner: AccountId,
    name: String,
    guardians: Vec<AccountId>,
    accepted: Vec<AccountId>,
    /// When each guardian's invitation expires (same order as `guardians`).
    invite_expires_ns: Vec<u64>,
    /// How long a new or renewed invitation stays valid.
    invite_validity: TimeSpan,
    threshold: u8,
    check_in_interval: TimeSpan,
    silence_period: TimeSpan,
    waiting_period: TimeSpan,
    hidden_heirs: bool,
    commitments: Vec<String>,
    heirs: Vec<Heir>,
    last_check_in_ns: u64,
    release: Option<Release>,
    derivation_path: String,
    chain_id: u64,
}

/// The contract itself: deployed once, holds many vaults.
#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct InheritanceVault {
    /// The MPC signer contract, shared by all vaults.
    signer: AccountId,
    next_id: u64,
    vaults: LookupMap<u64, VaultData>,
    by_owner: LookupMap<AccountId, Vec<u64>>,
    by_guardian: LookupMap<AccountId, Vec<u64>>,
}

// ------------------------------------------------------------------ outputs

#[near(serializers = [json])]
pub struct Rules {
    pub vault_id: u64,
    pub name: String,
    pub owner: AccountId,
    pub guardians: Vec<AccountId>,
    pub guardians_accepted: Vec<AccountId>,
    pub guardian_invites: Vec<GuardianInvite>,
    pub invite_validity: TimeSpan,
    /// False until the vault has at least one guardian and one heir.
    pub setup_complete: bool,
    pub threshold: u8,
    pub check_in_interval: TimeSpan,
    pub silence_period: TimeSpan,
    pub waiting_period: TimeSpan,
    pub check_in_interval_seconds: u64,
    pub silence_period_seconds: u64,
    pub waiting_period_seconds: u64,
    pub heirs_hidden: bool,
    pub heir_commitments: Vec<String>,
    pub heirs: Vec<Heir>,
    pub signer_contract: AccountId,
    pub derivation_path: String,
    pub ethereum_chain_id: u64,
}

/// One guardian's invitation: `accepted`, `invited` (still valid) or `expired`.
#[near(serializers = [json])]
pub struct GuardianInvite {
    pub account: AccountId,
    pub state: String,
    pub expires_at_ns: u64,
}

#[near(serializers = [json])]
pub struct Status {
    /// inactive | alive | guardians_may_act | pending_approval | countdown | executable
    pub phase: String,
    pub now_ns: u64,
    pub last_check_in_ns: u64,
    pub next_check_in_due_ns: u64,
    pub silence_ends_ns: u64,
    pub approvals: Vec<AccountId>,
    pub executable_at_ns: Option<u64>,
    pub execution_started: bool,
    /// The Ethereum nonce of the first payout, once the numbers are fixed.
    /// Lets a client tell whether the payouts have already gone out.
    pub execution_base_nonce: Option<u64>,
}

/// One line in a "My vaults" list.
#[near(serializers = [json])]
pub struct VaultSummary {
    pub vault_id: u64,
    pub name: String,
    pub owner: AccountId,
}

#[near(serializers = [json])]
pub struct SignedPayout {
    pub vault_id: u64,
    pub heir_index: u32,
    pub to: String,
    pub amount_wei: U128,
    pub nonce: u64,
    pub unsigned_tx_hex: String,
    /// Raw MPC response (r, s, recovery id). Finalize and broadcast off-chain.
    pub signature: Value,
}

// ----------------------------------------------------------------- contract

#[near]
impl InheritanceVault {
    /// Deploy once. `signer` is the MPC contract (v1.signer-prod.testnet).
    #[init]
    pub fn new(signer: AccountId) -> Self {
        Self {
            signer,
            next_id: 1,
            vaults: LookupMap::new(b"v".to_vec()),
            by_owner: LookupMap::new(b"o".to_vec()),
            by_guardian: LookupMap::new(b"g".to_vec()),
        }
    }

    /// Create a new vault. The caller becomes its owner. Public heirs OR hidden
    /// heir commitments, never both. Attach a little NEAR to pay for the
    /// storage; whatever is not needed is sent back. Returns the vault id.
    #[payable]
    #[allow(clippy::too_many_arguments)]
    pub fn create_vault(
        &mut self,
        name: String,
        guardians: Vec<AccountId>,
        threshold: u8,
        check_in_interval: TimeSpan,
        silence_period: TimeSpan,
        waiting_period: TimeSpan,
        invite_expiry: TimeSpan,
        public_heirs: Vec<Heir>,
        heir_commitments: Vec<String>,
        chain_id: u64,
    ) -> u64 {
        let owner = env::predecessor_account_id();
        let deposit = env::attached_deposit().as_yoctonear();
        let storage_before = env::storage_usage();

        let id = self.next_id;
        self.next_id += 1;
        let data = VaultData::checked_new(
            owner.clone(),
            id,
            name,
            guardians,
            threshold,
            check_in_interval,
            silence_period,
            waiting_period,
            invite_expiry,
            public_heirs,
            heir_commitments,
            chain_id,
        );

        push_index(&mut self.by_owner, &owner, id);
        for g in &data.guardians {
            push_index(&mut self.by_guardian, g, id);
        }
        self.vaults.insert(id, data);
        self.charge_storage(storage_before, deposit, owner);
        env::log_str(&format!("vault_created:{}", id));
        id
    }

    // ------------------------------------------------- owner: edit the setup

    /// Owner only. Rename the organization at any time.
    #[payable]
    pub fn rename_vault(&mut self, vault_id: u64, name: String) {
        let (deposit, before) = (env::attached_deposit().as_yoctonear(), env::storage_usage());
        self.vault_mut(vault_id).rename(name);
        self.charge_storage(before, deposit, env::predecessor_account_id());
    }

    /// Owner only. Replace the guardian list and threshold (an empty list
    /// removes them all). Guardians who stay keep their acceptance, new ones
    /// get a fresh invitation. Not allowed while a release is pending.
    #[payable]
    pub fn set_guardians(&mut self, vault_id: u64, guardians: Vec<AccountId>, threshold: u8) {
        let (deposit, before) = (env::attached_deposit().as_yoctonear(), env::storage_usage());
        let old = self.vault(vault_id).guardians.clone();
        self.vault_mut(vault_id).set_guardians(guardians.clone(), threshold);
        for g in &old {
            if !guardians.contains(g) {
                remove_index(&mut self.by_guardian, g, vault_id);
            }
        }
        for g in &guardians {
            if !old.contains(g) {
                push_index(&mut self.by_guardian, g, vault_id);
            }
        }
        self.charge_storage(before, deposit, env::predecessor_account_id());
    }

    /// Owner only. Replace the heirs: public heirs OR hidden commitments, or
    /// neither to clear them. Not allowed while a release is pending.
    #[payable]
    pub fn set_heirs(&mut self, vault_id: u64, public_heirs: Vec<Heir>, heir_commitments: Vec<String>) {
        let (deposit, before) = (env::attached_deposit().as_yoctonear(), env::storage_usage());
        self.vault_mut(vault_id).set_heirs(public_heirs, heir_commitments);
        self.charge_storage(before, deposit, env::predecessor_account_id());
    }

    /// Owner only. Move one public heir to a new Ethereum address, keeping
    /// their share and name. For example when they get a new wallet. Not
    /// allowed while a release is pending.
    #[payable]
    pub fn update_heir_address(&mut self, vault_id: u64, old_address: String, new_address: String) {
        let (deposit, before) = (env::attached_deposit().as_yoctonear(), env::storage_usage());
        self.vault_mut(vault_id).update_heir_address(old_address, new_address);
        self.charge_storage(before, deposit, env::predecessor_account_id());
    }

    /// Owner only. Change the name shown for one public heir's address. This
    /// never changes who gets paid or how much.
    #[payable]
    pub fn rename_heir(&mut self, vault_id: u64, eth_address: String, label: String) {
        let (deposit, before) = (env::attached_deposit().as_yoctonear(), env::storage_usage());
        self.vault_mut(vault_id).rename_heir(eth_address, label);
        self.charge_storage(before, deposit, env::predecessor_account_id());
    }

    // ------------------------------------------------------------ guardians

    /// A guardian signs up for the role. The vault stays inactive until all
    /// of them have, which catches wrong or unreachable guardians early.
    pub fn accept_guardianship(&mut self, vault_id: u64) {
        self.vault_mut(vault_id).accept_guardianship();
    }

    /// Owner only. Sends a fresh invitation to a guardian who has not accepted
    /// yet, for example because the first one expired or went to the wrong
    /// account. Pass no `new_guardian` to invite the same account again (the
    /// clock restarts), or a different account to replace it. Guardians who
    /// have already accepted cannot be replaced.
    pub fn reinvite_guardian(&mut self, vault_id: u64, guardian: AccountId, new_guardian: Option<AccountId>) {
        let target = self.vault_mut(vault_id).reinvite_guardian(guardian.clone(), new_guardian);
        if target != guardian {
            remove_index(&mut self.by_guardian, &guardian, vault_id);
            push_index(&mut self.by_guardian, &target, vault_id);
        }
    }

    /// A guardian says the owner is gone. Allowed only after the silence
    /// period. Reaching the threshold starts the countdown; it does not pay.
    pub fn approve_release(&mut self, vault_id: u64) {
        self.vault_mut(vault_id).approve_release();
    }

    /// Hidden-heir mode: reveal the heirs so payouts can be built. Anyone can
    /// call it once a release is pending, but every entry must match a
    /// commitment the owner stored, so nobody can invent heirs.
    pub fn reveal_heirs(&mut self, vault_id: u64, heirs: Vec<Heir>) {
        self.vault_mut(vault_id).reveal_heirs(heirs);
    }

    // ---------------------------------------------------------------- owner

    pub fn check_in(&mut self, vault_id: u64) {
        self.vault_mut(vault_id).check_in();
    }

    pub fn cancel_release(&mut self, vault_id: u64) {
        self.vault_mut(vault_id).cancel_release();
    }

    // ------------------------------------------------------------- payouts

    /// Step 1 after the countdown: fix the numbers once. The caller supplies
    /// the vault's ETH balance and nonce (a NEAR contract cannot read
    /// Ethereum). The contract fixes the destinations and shares itself.
    pub fn start_execution(
        &mut self,
        vault_id: u64,
        base_nonce: u64,
        total_wei: U128,
        max_fee_per_gas: U128,
        max_priority_fee_per_gas: U128,
    ) {
        self.vault_mut(vault_id)
            .start_execution(base_nonce, total_wei, max_fee_per_gas, max_priority_fee_per_gas);
    }

    /// Guardian only: clears the fixed numbers after a failed payout, so
    /// the release can run again with the vault's current balance and nonce.
    /// With several heirs, a retry after a partial payout splits what is left
    /// by the same shares, so check what was paid first.
    pub fn reset_execution(&mut self, vault_id: u64) {
        self.vault_mut(vault_id).reset_execution();
    }

    /// Owner only: puts a released vault back to normal so it can be used
    /// again. Works only once the release has really started (the numbers are
    /// fixed), so it can never be used as a late cancel of the countdown.
    /// Guardians, heirs and the Ethereum address stay the same.
    pub fn restart_vault(&mut self, vault_id: u64) {
        self.vault_mut(vault_id).restart_vault();
    }

    /// Step 2: ask the MPC network to sign the payout for one heir. Call once
    /// per heir, attaching about 300 TGas. Repeating a call re-signs the same
    /// transaction (same nonce), so it can never double-pay.
    pub fn sign_payout(&mut self, vault_id: u64, heir_index: u32) -> Promise {
        let (unsigned, hash, amount, nonce, to, path) = self.vault(vault_id).build_payout(heir_index);

        // The one place that depends on the deployed MPC contract's argument
        // format. Check it against the contract's current ABI before deploying.
        let args = sign_request_args(&hash, &path);
        Promise::new(self.signer.clone())
            .function_call(
                "sign".to_string(),
                args.to_string().into_bytes(),
                NearToken::from_yoctonear(1),
                SIGN_GAS,
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALLBACK_GAS)
                    .on_signed(vault_id, heir_index, to, U128(amount), nonce, hex::encode(&unsigned)),
            )
    }

    #[private]
    pub fn on_signed(
        &mut self,
        vault_id: u64,
        heir_index: u32,
        to: String,
        amount_wei: U128,
        nonce: u64,
        unsigned_tx_hex: String,
        #[callback_result] result: Result<Value, PromiseError>,
    ) -> SignedPayout {
        let signature = result.unwrap_or_else(|_| env::panic_str("the MPC network did not sign; try again"));
        SignedPayout { vault_id, heir_index, to, amount_wei, nonce, unsigned_tx_hex, signature }
    }

    // ---------------------------------------------------------------- views

    pub fn get_rules(&self, vault_id: u64) -> Rules {
        self.vault(vault_id).rules(vault_id, &self.signer)
    }

    pub fn get_status(&self, vault_id: u64) -> Status {
        self.vault(vault_id).status()
    }

    /// Vaults this account owns (the "My vaults" list).
    pub fn get_vaults_by_owner(&self, account_id: AccountId) -> Vec<VaultSummary> {
        self.summaries(self.by_owner.get(&account_id))
    }

    /// Vaults this account was named a guardian of (invitations and duties).
    pub fn get_vaults_for_guardian(&self, account_id: AccountId) -> Vec<VaultSummary> {
        self.summaries(self.by_guardian.get(&account_id))
    }

    pub fn get_vault_count(&self) -> u64 {
        self.next_id - 1
    }

    pub fn get_signer(&self) -> AccountId {
        self.signer.clone()
    }

    /// Helper for the setup form so the commitment is computed exactly the
    /// way the contract checks it.
    pub fn compute_commitment(&self, heir: Heir) -> String {
        commitment(&heir)
    }
}

// ----------------------------------------------------------------- internal

/// The MPC signer's key domain for Ethereum (Secp256k1). Verified against
/// `v1.signer-prod.testnet` (v3.18.0): domain 0 is the Secp256k1 key.
const SIGN_DOMAIN_ID: u64 = 0;

/// Arguments for the signer's `sign` method, in its current (non-deprecated)
/// form: `{"request": {"payload_v2": {"Ecdsa": "<64 hex>"}, "path", "domain_id"}}`.
/// The signer derives the signing key from the calling account (this contract)
/// and `path`, so every vault (`vault-<id>`) signs with its own key.
fn sign_request_args(hash: &[u8; 32], path: &str) -> Value {
    json!({
        "request": {
            "payload_v2": { "Ecdsa": hex::encode(hash) },
            "path": path,
            "domain_id": SIGN_DOMAIN_ID,
        }
    })
}

fn push_index(map: &mut LookupMap<AccountId, Vec<u64>>, who: &AccountId, id: u64) {
    let mut list = map.get(who).cloned().unwrap_or_default();
    list.push(id);
    map.insert(who.clone(), list);
}

fn remove_index(map: &mut LookupMap<AccountId, Vec<u64>>, who: &AccountId, id: u64) {
    if let Some(list) = map.get(who) {
        let list: Vec<u64> = list.iter().copied().filter(|x| *x != id).collect();
        if list.is_empty() {
            map.remove(who);
        } else {
            map.insert(who.clone(), list);
        }
    }
}

impl InheritanceVault {
    /// Whoever changed the setup pays for any extra storage it needs. Whatever
    /// they attached beyond that is sent back.
    fn charge_storage(&mut self, storage_before: u64, deposit: u128, payer: AccountId) {
        self.vaults.flush();
        self.by_owner.flush();
        self.by_guardian.flush();
        let used = env::storage_usage().saturating_sub(storage_before) as u128;
        let cost = env::storage_byte_cost().as_yoctonear() * used;
        require!(
            deposit >= cost,
            format!("attach at least {} yoctoNEAR to pay for storage", cost)
        );
        if deposit > cost {
            let _ = Promise::new(payer).transfer(NearToken::from_yoctonear(deposit - cost));
        }
    }

    fn vault(&self, id: u64) -> &VaultData {
        self.vaults.get(&id).unwrap_or_else(|| env::panic_str("vault not found"))
    }

    fn vault_mut(&mut self, id: u64) -> &mut VaultData {
        self.vaults.get_mut(&id).unwrap_or_else(|| env::panic_str("vault not found"))
    }

    fn summaries(&self, ids: Option<&Vec<u64>>) -> Vec<VaultSummary> {
        ids.map(|ids| {
            ids.iter()
                .map(|id| {
                    let v = self.vault(*id);
                    VaultSummary { vault_id: *id, name: v.name.clone(), owner: v.owner.clone() }
                })
                .collect()
        })
        .unwrap_or_default()
    }
}

impl VaultData {
    #[allow(clippy::too_many_arguments)]
    fn checked_new(
        owner: AccountId,
        id: u64,
        name: String,
        guardians: Vec<AccountId>,
        threshold: u8,
        check_in_interval: TimeSpan,
        silence_period: TimeSpan,
        waiting_period: TimeSpan,
        invite_expiry: TimeSpan,
        public_heirs: Vec<Heir>,
        heir_commitments: Vec<String>,
        chain_id: u64,
    ) -> Self {
        check_name(&name);
        check_guardians(&guardians, threshold, &owner);
        let threshold = if guardians.is_empty() { 0 } else { threshold };
        let interval = check_in_interval.to_ns();
        let silence = silence_period.to_ns();
        waiting_period.to_ns();
        require!(silence >= interval, "silence period must be at least the check-in interval");

        let (hidden, heirs, commitments) = resolve_heirs(public_heirs, heir_commitments);
        let invite_expires_ns = vec![env::block_timestamp() + invite_expiry.to_ns(); guardians.len()];

        Self {
            owner,
            name,
            guardians,
            accepted: vec![],
            invite_expires_ns,
            invite_validity: invite_expiry,
            threshold,
            check_in_interval,
            silence_period,
            waiting_period,
            hidden_heirs: hidden,
            commitments,
            heirs,
            last_check_in_ns: env::block_timestamp(),
            release: None,
            // A different path per vault gives each vault its own Ethereum address.
            derivation_path: format!("vault-{}", id),
            chain_id,
        }
    }

    fn accept_guardianship(&mut self) {
        let g = env::predecessor_account_id();
        let i = self
            .guardians
            .iter()
            .position(|x| *x == g)
            .unwrap_or_else(|| env::panic_str("you are not a guardian of this vault"));
        require!(!self.accepted.contains(&g), "already accepted");
        require!(
            env::block_timestamp() <= self.invite_expires_ns[i],
            "your invitation has expired; ask the owner to invite you again"
        );
        let was_ready = self.ready();
        self.accepted.push(g);
        env::log_str("guardian_accepted");
        self.refresh_activation(was_ready);
    }

    // ---- owner edits (guardians, heirs, names)

    fn require_editable(&self) {
        self.only_owner();
        require!(
            self.release.is_none(),
            "guardians and heirs cannot be changed while a release is pending; cancel it first"
        );
    }

    fn rename(&mut self, name: String) {
        self.only_owner();
        check_name(&name);
        self.name = name;
    }

    fn set_guardians(&mut self, new_guardians: Vec<AccountId>, threshold: u8) {
        self.require_editable();
        check_guardians(&new_guardians, threshold, &self.owner);
        let was_ready = self.ready();
        let fresh =env::block_timestamp() + self.invite_validity.to_ns();
        let mut expires = Vec::new();
        let mut accepted = Vec::new();
        for g in &new_guardians {
            match self.guardians.iter().position(|x| x == g) {
                Some(i) => {
                    expires.push(self.invite_expires_ns[i]);
                    if self.accepted.contains(g) {
                        accepted.push(g.clone());
                    }
                }
                None => expires.push(fresh),
            }
        }
        self.threshold = if new_guardians.is_empty() { 0 } else { threshold };
        self.guardians = new_guardians;
        self.invite_expires_ns = expires;
        self.accepted = accepted;
        // Changing the setup proves the owner is around, so it counts as a check-in.
        self.last_check_in_ns = env::block_timestamp();
        env::log_str("guardians_updated");
        self.refresh_activation(was_ready);
    }

    fn set_heirs(&mut self, public_heirs: Vec<Heir>, heir_commitments: Vec<String>) {
        self.require_editable();
        let was_ready = self.ready();
        let (hidden, mut heirs, commitments) = resolve_heirs(public_heirs, heir_commitments);
        // An heir who stays keeps their name unless a new one is sent.
        for h in heirs.iter_mut() {
            if h.label.is_empty() {
                if let Some(old) = self.heirs.iter().find(|o| o.eth_address == h.eth_address) {
                    h.label = old.label.clone();
                }
            }
        }
        self.hidden_heirs = hidden;
        self.heirs = heirs;
        self.commitments = commitments;
        // Changing the setup proves the owner is around, so it counts as a check-in.
        self.last_check_in_ns = env::block_timestamp();
        env::log_str("heirs_updated");
        self.refresh_activation(was_ready);
    }

    fn update_heir_address(&mut self, old_address: String, new_address: String) {
        self.require_editable();
        let old = normalize_address(&old_address);
        let new = normalize_address(&new_address);
        require!(old != new, "the new address is the same as the old one");
        require!(
            !self.heirs.iter().any(|h| h.eth_address == new),
            "duplicate heir address"
        );
        let heir = self
            .heirs
            .iter_mut()
            .find(|h| h.eth_address == old)
            .unwrap_or_else(|| env::panic_str("no heir with that address"));
        heir.eth_address = new;
        // Changing the setup proves the owner is around, so it counts as a check-in.
        self.last_check_in_ns = env::block_timestamp();
        env::log_str("heir_address_updated");
    }

    fn rename_heir(&mut self, eth_address: String, label: String) {
        self.only_owner();
        require!(label.len() <= MAX_LABEL, "an heir name can be at most 64 characters");
        let addr = normalize_address(&eth_address);
        let heir = self
            .heirs
            .iter_mut()
            .find(|h| h.eth_address == addr)
            .unwrap_or_else(|| env::panic_str("no heir with that address"));
        heir.label = label.trim().to_string();
        env::log_str("heir_renamed");
    }

    /// When a vault becomes ready for a release (guardians and heirs set, all
    /// guardians accepted), the silence clock restarts, so guardians cannot
    /// act right away on a vault that was created long before it was set up.
    fn refresh_activation(&mut self, was_ready: bool) {
        if !was_ready && self.ready() {
            self.last_check_in_ns = env::block_timestamp();
            env::log_str("vault_ready");
        }
    }

    /// Needs at least one guardian and at least one heir (or heir hash).
    fn setup_complete(&self) -> bool {
        !self.guardians.is_empty() && (!self.heirs.is_empty() || !self.commitments.is_empty())
    }

    /// Returns the account that now holds the invitation.
    fn reinvite_guardian(&mut self, guardian: AccountId, new_guardian: Option<AccountId>) -> AccountId {
        self.only_owner();
        let i = self
            .guardians
            .iter()
            .position(|x| *x == guardian)
            .unwrap_or_else(|| env::panic_str("not a guardian of this vault"));
        require!(!self.accepted.contains(&guardian), "this guardian has already accepted");
        let target = new_guardian.unwrap_or_else(|| guardian.clone());
        require!(target != self.owner, "the owner cannot be a guardian");
        if target != guardian {
            require!(!self.guardians.contains(&target), "duplicate guardian");
            self.guardians[i] = target.clone();
        }
        self.invite_expires_ns[i] = env::block_timestamp() + self.invite_validity.to_ns();
        env::log_str("guardian_reinvited");
        target
    }

    fn guardian_invites(&self) -> Vec<GuardianInvite> {
        let now = env::block_timestamp();
        self.guardians
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let state = if self.accepted.contains(g) {
                    "accepted"
                } else if now > self.invite_expires_ns[i] {
                    "expired"
                } else {
                    "invited"
                };
                GuardianInvite {
                    account: g.clone(),
                    state: state.to_string(),
                    expires_at_ns: self.invite_expires_ns[i],
                }
            })
            .collect()
    }

    fn approve_release(&mut self) {
        require!(self.setup_complete(), "the vault setup is not finished: add guardians and heirs first");
        require!(self.is_active(), "vault is not active: not every guardian has accepted yet");
        let g = env::predecessor_account_id();
        require!(self.guardians.contains(&g), "only a guardian can approve");
        let now = env::block_timestamp();
        require!(now >= self.silence_ends_ns(), "the owner has not been silent long enough");

        let threshold = self.threshold as usize;
        let waiting = self.waiting_period.to_ns();
        let rel = self.release.get_or_insert(Release {
            approvals: vec![],
            started_at_ns: now,
            executable_at_ns: None,
            plan: None,
        });
        require!(!rel.approvals.contains(&g), "you already approved");
        rel.approvals.push(g);
        if rel.executable_at_ns.is_none() && rel.approvals.len() >= threshold {
            rel.executable_at_ns = Some(now + waiting);
            env::log_str("countdown_started");
        }
    }

    fn reveal_heirs(&mut self, heirs: Vec<Heir>) {
        require!(self.hidden_heirs, "heirs are already public");
        require!(self.release.is_some(), "heirs are revealed only once a release is pending");
        require!(self.heirs.is_empty(), "heirs already revealed");
        // Anyone may call this, so names are not accepted here; the owner can
        // name the revealed heirs afterwards.
        let mut heirs = check_heirs(&heirs);
        for h in heirs.iter_mut() {
            h.label.clear();
        }
        require!(heirs.len() == self.commitments.len(), "wrong number of heirs");
        let mut remaining = self.commitments.clone();
        for h in &heirs {
            let c = commitment(h);
            let pos = remaining.iter().position(|x| *x == c);
            require!(pos.is_some(), "an heir does not match any stored commitment");
            remaining.remove(pos.unwrap());
        }
        self.heirs = heirs;
        env::log_str("heirs_revealed");
    }

    fn check_in(&mut self) {
        self.only_owner();
        require!(!self.release_is_executable(), "the waiting period has ended; the release can no longer be cancelled");
        self.last_check_in_ns = env::block_timestamp();
        if self.release.take().is_some() {
            env::log_str("release_cancelled_by_check_in");
        }
        env::log_str("check_in");
    }

    fn cancel_release(&mut self) {
        self.only_owner();
        require!(self.release.is_some(), "no pending release");
        require!(!self.release_is_executable(), "the waiting period has ended; the release can no longer be cancelled");
        self.release = None;
        env::log_str("release_cancelled");
    }

    fn start_execution(
        &mut self,
        base_nonce: u64,
        total_wei: U128,
        max_fee_per_gas: U128,
        max_priority_fee_per_gas: U128,
    ) {
        self.only_guardian();
        require!(self.release_is_executable(), "the countdown has not finished");
        require!(!self.heirs.is_empty(), "heirs have not been revealed yet");
        require!(max_priority_fee_per_gas.0 <= max_fee_per_gas.0, "priority fee cannot exceed max fee");
        let plan = ExecutionPlan { base_nonce, total_wei, max_fee_per_gas, max_priority_fee_per_gas };
        // Fails if the balance cannot even cover the gas.
        self.payout_amount(&plan, 0);
        let rel = self.release.as_mut().unwrap();
        require!(rel.plan.is_none(), "execution already started");
        rel.plan = Some(plan);
        env::log_str("execution_started");
    }

    fn reset_execution(&mut self) {
        self.only_guardian();
        require!(self.release_is_executable(), "the countdown has not finished");
        let rel = self.release.as_mut().unwrap();
        require!(rel.plan.is_some(), "execution has not started");
        rel.plan = None;
        env::log_str("execution_reset");
    }

    fn restart_vault(&mut self) {
        self.only_owner();
        require!(self.release_is_executable(), "the countdown has not finished");
        require!(
            self.release.as_ref().is_some_and(|r| r.plan.is_some()),
            "the release has not started, so the vault cannot be restarted"
        );
        self.release = None;
        self.last_check_in_ns = env::block_timestamp();
        env::log_str("vault_restarted");
    }

    /// Builds the unsigned transaction for one heir:
    /// (unsigned bytes, keccak hash, amount, nonce, heir address, signing path).
    fn build_payout(&self, heir_index: u32) -> (Vec<u8>, [u8; 32], u128, u64, String, String) {
        self.only_guardian();
        let plan = self
            .release
            .as_ref()
            .and_then(|r| r.plan.clone())
            .unwrap_or_else(|| env::panic_str("call start_execution first"));
        require!((heir_index as usize) < self.heirs.len(), "no such heir");
        let heir = &self.heirs[heir_index as usize];
        let to = address_bytes(&heir.eth_address);
        let amount = self.payout_amount(&plan, heir_index as usize);
        let nonce = plan.base_nonce + heir_index as u64;
        let unsigned = eip1559_unsigned(
            self.chain_id,
            nonce,
            plan.max_priority_fee_per_gas.0,
            plan.max_fee_per_gas.0,
            TRANSFER_GAS_LIMIT,
            &to,
            amount,
        );
        let hash = env::keccak256_array(&unsigned);
        (unsigned, hash, amount, nonce, heir.eth_address.clone(), self.derivation_path.clone())
    }

    fn rules(&self, vault_id: u64, signer: &AccountId) -> Rules {
        Rules {
            vault_id,
            name: self.name.clone(),
            owner: self.owner.clone(),
            guardians: self.guardians.clone(),
            guardians_accepted: self.accepted.clone(),
            guardian_invites: self.guardian_invites(),
            invite_validity: self.invite_validity,
            setup_complete: self.setup_complete(),
            threshold: self.threshold,
            check_in_interval: self.check_in_interval,
            silence_period: self.silence_period,
            waiting_period: self.waiting_period,
            check_in_interval_seconds: self.check_in_interval.to_ns() / 1_000_000_000,
            silence_period_seconds: self.silence_period.to_ns() / 1_000_000_000,
            waiting_period_seconds: self.waiting_period.to_ns() / 1_000_000_000,
            heirs_hidden: self.hidden_heirs,
            heir_commitments: self.commitments.clone(),
            heirs: self.heirs.clone(),
            signer_contract: signer.clone(),
            derivation_path: self.derivation_path.clone(),
            ethereum_chain_id: self.chain_id,
        }
    }

    fn status(&self) -> Status {
        let now = env::block_timestamp();
        let exec_at = self.release.as_ref().and_then(|r| r.executable_at_ns);
        let phase = if !self.is_active() {
            // Guardians were named but have not all accepted yet.
            "inactive"
        } else if !self.setup_complete() {
            // Nothing can be released yet: no guardians or no heirs.
            "alive"
        } else if let Some(t) = exec_at {
            if now >= t { "executable" } else { "countdown" }
        } else if self.release.is_some() {
            "pending_approval"
        } else if now >= self.silence_ends_ns() {
            "guardians_may_act"
        } else {
            "alive"
        };
        Status {
            phase: phase.to_string(),
            now_ns: now,
            last_check_in_ns: self.last_check_in_ns,
            next_check_in_due_ns: self.last_check_in_ns + self.check_in_interval.to_ns(),
            silence_ends_ns: self.silence_ends_ns(),
            approvals: self.release.as_ref().map(|r| r.approvals.clone()).unwrap_or_default(),
            executable_at_ns: exec_at,
            execution_started: self.release.as_ref().map(|r| r.plan.is_some()).unwrap_or(false),
            execution_base_nonce: self.release.as_ref().and_then(|r| r.plan.as_ref()).map(|p| p.base_nonce),
        }
    }

    /// The release is carried out by guardians, never by the owner.
    fn only_guardian(&self) {
        require!(
            self.guardians.contains(&env::predecessor_account_id()),
            "only a guardian can carry out the release"
        );
    }

    fn only_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "only the owner can do this");
    }

    /// Every guardian that was named has accepted. A vault with no guardians
    /// yet counts as active: the owner can check in from day one.
    fn is_active(&self) -> bool {
        self.accepted.len() == self.guardians.len()
    }

    /// A release is possible only when there is at least one guardian and one
    /// heir, and every guardian has accepted.
    fn ready(&self) -> bool {
        self.setup_complete() && self.is_active()
    }

    fn silence_ends_ns(&self) -> u64 {
        self.last_check_in_ns + self.silence_period.to_ns()
    }

    fn release_is_executable(&self) -> bool {
        match self.release.as_ref().and_then(|r| r.executable_at_ns) {
            Some(t) => env::block_timestamp() >= t,
            None => false,
        }
    }

    /// Heir's payout: (total - gas for every payout) * share / 10000.
    /// Rounding dust stays in the vault.
    fn payout_amount(&self, plan: &ExecutionPlan, index: usize) -> u128 {
        let n = self.heirs.len() as u128;
        let fee_total = (TRANSFER_GAS_LIMIT as u128)
            .checked_mul(plan.max_fee_per_gas.0)
            .and_then(|x| x.checked_mul(n))
            .unwrap_or_else(|| env::panic_str("fee too large"));
        require!(plan.total_wei.0 > fee_total, "the balance does not cover the gas for the payouts");
        let distributable = plan.total_wei.0 - fee_total;
        let share = self.heirs[index].bps as u128;
        distributable
            .checked_mul(share)
            .unwrap_or_else(|| env::panic_str("balance too large"))
            / 10_000
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::testing_env;

    const MIN: u64 = 60 * 1_000_000_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn ctx(who: &str, at_ns: u64) {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(who))
            .current_account_id(acc("vault.joseph.testnet"))
            .block_timestamp(at_ns)
            .build());
    }

    /// Like ctx, but attaches NEAR (needed for create_vault's storage cost).
    fn ctx_paying(who: &str, at_ns: u64) {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(who))
            .current_account_id(acc("vault.joseph.testnet"))
            .block_timestamp(at_ns)
            .attached_deposit(NearToken::from_near(1))
            .build());
    }

    fn span(amount: u32, unit: Unit) -> TimeSpan {
        TimeSpan { amount, unit }
    }

    fn heir(addr: &str, bps: u16, salt: &str) -> Heir {
        Heir { eth_address: addr.to_string(), bps, salt: salt.to_string(), label: String::new() }
    }

    fn named(addr: &str, bps: u16, label: &str) -> Heir {
        Heir { eth_address: addr.to_string(), bps, salt: String::new(), label: label.to_string() }
    }

    const WIFE: &str = "0x1111111111111111111111111111111111111111";
    const BROTHER: &str = "0x2222222222222222222222222222222222222222";

    fn registry() -> InheritanceVault {
        ctx("deployer.testnet", 0);
        InheritanceVault::new(acc("v1.signer-prod.testnet"))
    }

    /// Creates a vault as `owner` with guardians g1..g3 (2 of 3), minute timers.
    fn create_as(c: &mut InheritanceVault, owner: &str, name: &str, guardians: &[&str], public: bool) -> u64 {
        let heirs = vec![heir(WIFE, 6000, "s1"), heir(BROTHER, 4000, "s2")];
        let (p, h) = if public {
            (heirs.iter().map(|h| heir(&h.eth_address, h.bps, "")).collect(), vec![])
        } else {
            (vec![], heirs.iter().map(commitment).collect())
        };
        ctx_paying(owner, 0);
        c.create_vault(
            name.into(),
            guardians.iter().map(|g| acc(g)).collect(),
            2,
            span(1, Unit::Minutes),
            span(2, Unit::Minutes),
            span(2, Unit::Minutes),
            span(10, Unit::Minutes),
            p,
            h,
            11_155_111,
        )
    }

    fn make(public: bool) -> (InheritanceVault, u64) {
        let mut c = registry();
        let id = create_as(&mut c, "joseph.testnet", "Family Vault", &["g1.testnet", "g2.testnet", "g3.testnet"], public);
        (c, id)
    }

    fn activate(c: &mut InheritanceVault, id: u64) {
        for g in ["g1.testnet", "g2.testnet", "g3.testnet"] {
            ctx(g, 0);
            c.accept_guardianship(id);
        }
    }

    #[test]
    fn units_convert_to_nanoseconds() {
        let s = 1_000_000_000u64;
        assert_eq!(span(5, Unit::Minutes).to_ns(), 300 * s);
        assert_eq!(span(2, Unit::Hours).to_ns(), 7_200 * s);
        assert_eq!(span(90, Unit::Days).to_ns(), 90 * 86_400 * s);
        assert_eq!(span(3, Unit::Months).to_ns(), 90 * 86_400 * s);
        assert_eq!(span(1, Unit::Years).to_ns(), 365 * 86_400 * s);
    }

    #[test]
    #[should_panic(expected = "at most 100 years")]
    fn timer_too_long_is_rejected() {
        span(101, Unit::Years).to_ns();
    }

    #[test]
    #[should_panic(expected = "longer than zero")]
    fn zero_timer_is_rejected() {
        span(0, Unit::Days).to_ns();
    }

    #[test]
    fn rlp_known_vectors() {
        assert_eq!(rlp_bytes(b"dog"), vec![0x83, b'd', b'o', b'g']);
        assert_eq!(rlp_uint(0), vec![0x80]);
        assert_eq!(rlp_uint(15), vec![0x0f]);
        assert_eq!(rlp_uint(1024), vec![0x82, 0x04, 0x00]);
        assert_eq!(rlp_list(&[]), vec![0xc0]);
        assert_eq!(
            rlp_list(&[rlp_bytes(b"cat"), rlp_bytes(b"dog")]),
            vec![0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g']
        );
        // long string form: 56 bytes -> 0xb8 0x38 ...
        let long = vec![b'a'; 56];
        let enc = rlp_bytes(&long);
        assert_eq!(&enc[..2], &[0xb8, 0x38]);
        assert_eq!(enc.len(), 58);
    }

    #[test]
    fn sign_request_matches_the_signers_current_format() {
        // Exactly the shape the live signer accepted on testnet (probe, Nov 2026).
        let args = sign_request_args(&[0x11u8; 32], "vault-1");
        assert_eq!(
            args,
            json!({
                "request": {
                    "payload_v2": { "Ecdsa": "1111111111111111111111111111111111111111111111111111111111111111" },
                    "path": "vault-1",
                    "domain_id": 0
                }
            })
        );
        // The deprecated field names must not be sent any more.
        assert!(args["request"].get("payload").is_none());
        assert!(args["request"].get("key_version").is_none());
    }

    #[test]
    fn dump_unsigned_tx_vector() {
        // Printed so the same transaction can be cross-checked with viem.
        let to = address_bytes(WIFE);
        let tx = eip1559_unsigned(11_155_111, 7, 1_000_000_000, 30_000_000_000, 21_000, &to, 123_456_789_000_000_000);
        println!("TXVECTOR {}", hex::encode(tx));
    }

    #[test]
    fn full_flow_with_public_heirs_and_cancel() {
        let (mut c, id) = make(true);
        assert_eq!(c.get_status(id).phase, "inactive");
        activate(&mut c, id);
        ctx("joseph.testnet", MIN);
        assert_eq!(c.get_status(id).phase, "alive");

        // After 2 minutes of silence the guardians may act.
        ctx("g1.testnet", 3 * MIN);
        assert_eq!(c.get_status(id).phase, "guardians_may_act");
        c.approve_release(id);
        assert_eq!(c.get_status(id).phase, "pending_approval");
        ctx("g2.testnet", 3 * MIN + 1);
        c.approve_release(id);
        let st = c.get_status(id);
        assert_eq!(st.phase, "countdown");
        assert_eq!(st.executable_at_ns, Some(3 * MIN + 1 + 2 * MIN));

        // The owner can still cancel during the countdown.
        ctx("joseph.testnet", 4 * MIN);
        c.cancel_release(id);
        assert_eq!(c.get_status(id).phase, "guardians_may_act");
        assert!(get_logs().iter().any(|l| l == "release_cancelled"));
    }

    #[test]
    fn check_in_cancels_pending_release() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("joseph.testnet", 3 * MIN + 10);
        c.check_in(id);
        let st = c.get_status(id);
        assert_eq!(st.phase, "alive");
        assert!(st.approvals.is_empty());
    }

    #[test]
    #[should_panic(expected = "not been silent long enough")]
    fn guardians_cannot_act_early() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", MIN);
        c.approve_release(id);
    }

    #[test]
    #[should_panic(expected = "not every guardian has accepted")]
    fn inactive_until_all_guardians_accept() {
        let (mut c, id) = make(true);
        ctx("g1.testnet", 0);
        c.accept_guardianship(id);
        ctx("g1.testnet", 10 * MIN);
        c.approve_release(id);
    }

    #[test]
    #[should_panic(expected = "can no longer be cancelled")]
    fn cannot_cancel_after_waiting_period() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g2.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("joseph.testnet", 5 * MIN + 1);
        c.cancel_release(id);
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn guardian_cannot_cancel() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        c.cancel_release(id);
    }

    #[test]
    fn hidden_heirs_reveal_and_amounts() {
        let (mut c, id) = make(false);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g2.testnet", 3 * MIN);
        c.approve_release(id);

        ctx("anyone.testnet", 5 * MIN + 1);
        c.reveal_heirs(id, vec![heir(BROTHER, 4000, "s2"), heir(WIFE, 6000, "s1")]);
        assert_eq!(c.vault(id).heirs.len(), 2);
        ctx("g1.testnet", 5 * MIN + 1);

        // 1 ETH balance, 30 gwei max fee, 2 payouts.
        let total: u128 = 1_000_000_000_000_000_000;
        c.start_execution(id, 5, U128(total), U128(30_000_000_000), U128(1_000_000_000));
        let v = c.vault(id);
        let plan = v.release.as_ref().unwrap().plan.clone().unwrap();
        let fee_total = TRANSFER_GAS_LIMIT as u128 * 30_000_000_000 * 2;
        let distributable = total - fee_total;
        let a0 = v.payout_amount(&plan, 0);
        let a1 = v.payout_amount(&plan, 1);
        // heirs were stored in the order revealed: brother first.
        assert_eq!(v.heirs[0].eth_address, BROTHER);
        assert_eq!(a0, distributable * 4000 / 10_000);
        assert_eq!(a1, distributable * 6000 / 10_000);
        assert!(a0 + a1 + fee_total <= total);
    }

    #[test]
    #[should_panic(expected = "does not match any stored commitment")]
    fn invented_heir_is_rejected() {
        let (mut c, id) = make(false);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("attacker.testnet", 3 * MIN);
        c.reveal_heirs(
            id,
            vec![heir("0x3333333333333333333333333333333333333333", 6000, "s1"), heir(BROTHER, 4000, "s2")],
        );
    }

    #[test]
    #[should_panic(expected = "add up to exactly 10000")]
    fn shares_must_total_100_percent() {
        let mut c = registry();
        ctx_paying("joseph.testnet", 0);
        c.create_vault(
            "V".into(),
            vec![acc("g1.testnet")],
            1,
            span(1, Unit::Days),
            span(2, Unit::Days),
            span(1, Unit::Days),
            span(7, Unit::Days),
            vec![heir(WIFE, 5000, ""), heir(BROTHER, 4000, "")],
            vec![],
            11_155_111,
        );
    }

    #[test]
    #[should_panic(expected = "cannot be a guardian")]
    fn owner_cannot_be_guardian() {
        let mut c = registry();
        ctx_paying("joseph.testnet", 0);
        c.create_vault(
            "V".into(),
            vec![acc("joseph.testnet")],
            1,
            span(1, Unit::Days),
            span(2, Unit::Days),
            span(1, Unit::Days),
            span(7, Unit::Days),
            vec![heir(WIFE, 10000, "")],
            vec![],
            11_155_111,
        );
    }

    // ---------------------------------------------------- many vaults, one contract

    const PRIVY_OWNER: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    #[test]
    fn the_caller_becomes_the_owner() {
        // A Privy-style implicit account creates a vault directly: no deploy needed.
        let mut c = registry();
        let id = create_as(&mut c, PRIVY_OWNER, "Mine", &["g1.testnet", "g2.testnet"], true);
        assert_eq!(c.get_rules(id).owner, acc(PRIVY_OWNER));
        ctx(PRIVY_OWNER, MIN);
        c.check_in(id);
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn someone_else_is_not_the_owner() {
        let mut c = registry();
        let id = create_as(&mut c, PRIVY_OWNER, "Mine", &["g1.testnet", "g2.testnet"], true);
        ctx("deployer.testnet", MIN);
        c.check_in(id);
    }

    #[test]
    fn each_vault_has_its_own_path_and_rules() {
        let mut c = registry();
        let family = create_as(&mut c, "joseph.testnet", "My Family", &["g1.testnet", "g2.testnet", "g3.testnet"], true);
        let work = create_as(&mut c, "joseph.testnet", "Workplace", &["w1.testnet", "w2.testnet"], true);
        assert_eq!((family, work), (1, 2));
        assert_eq!(c.get_vault_count(), 2);

        let (rf, rw) = (c.get_rules(family), c.get_rules(work));
        assert_eq!(rf.name, "My Family");
        assert_eq!(rw.name, "Workplace");
        // Different path => different Ethereum address for each vault.
        assert_eq!(rf.derivation_path, "vault-1");
        assert_eq!(rw.derivation_path, "vault-2");
        assert_ne!(rf.derivation_path, rw.derivation_path);
        assert_eq!(rf.guardians.len(), 3);
        assert_eq!(rw.guardians.len(), 2);
        assert_eq!(rf.signer_contract, acc("v1.signer-prod.testnet"));
    }

    #[test]
    fn vaults_do_not_affect_each_other() {
        let mut c = registry();
        let family = create_as(&mut c, "joseph.testnet", "My Family", &["g1.testnet", "g2.testnet"], true);
        let work = create_as(&mut c, "joseph.testnet", "Workplace", &["w1.testnet", "w2.testnet"], true);
        for g in ["g1.testnet", "g2.testnet"] {
            ctx(g, 0);
            c.accept_guardianship(family);
        }
        assert_eq!(c.get_status(family).phase, "alive");
        // Workplace is still waiting for its own guardians.
        assert_eq!(c.get_status(work).phase, "inactive");

        // Checking in on one vault leaves the other's clock alone.
        ctx("joseph.testnet", 10 * MIN);
        c.check_in(family);
        assert_eq!(c.get_status(family).last_check_in_ns, 10 * MIN);
        assert_eq!(c.get_status(work).last_check_in_ns, 0);
    }

    #[test]
    #[should_panic(expected = "only a guardian can approve")]
    fn a_guardian_of_one_vault_cannot_act_on_another() {
        let mut c = registry();
        let family = create_as(&mut c, "joseph.testnet", "My Family", &["g1.testnet", "g2.testnet"], true);
        let work = create_as(&mut c, "joseph.testnet", "Workplace", &["w1.testnet", "w2.testnet"], true);
        for g in ["g1.testnet", "g2.testnet"] {
            ctx(g, 0);
            c.accept_guardianship(family);
        }
        for g in ["w1.testnet", "w2.testnet"] {
            ctx(g, 0);
            c.accept_guardianship(work);
        }
        // g1 guards the family vault only.
        ctx("g1.testnet", 10 * MIN);
        c.approve_release(work);
    }

    #[test]
    fn lists_for_owner_and_guardian() {
        let mut c = registry();
        create_as(&mut c, "joseph.testnet", "My Family", &["g1.testnet", "g2.testnet"], true);
        create_as(&mut c, "joseph.testnet", "Workplace", &["g1.testnet", "w2.testnet"], true);
        create_as(&mut c, "other.testnet", "Other", &["g3.testnet", "g4.testnet"], true);

        let mine = c.get_vaults_by_owner(acc("joseph.testnet"));
        assert_eq!(mine.iter().map(|v| v.vault_id).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(mine[1].name, "Workplace");
        assert_eq!(c.get_vaults_by_owner(acc("other.testnet")).len(), 1);
        assert!(c.get_vaults_by_owner(acc("nobody.testnet")).is_empty());

        // g1 guards two vaults, g3 guards one.
        assert_eq!(c.get_vaults_for_guardian(acc("g1.testnet")).len(), 2);
        assert_eq!(c.get_vaults_for_guardian(acc("g3.testnet")).len(), 1);
    }

    #[test]
    #[should_panic(expected = "vault not found")]
    fn unknown_vault_is_rejected() {
        let c = registry();
        c.get_status(99);
    }

    // ------------------------------------------------------------- invitations

    fn state_of(c: &InheritanceVault, id: u64, who: &str) -> String {
        c.get_rules(id).guardian_invites.into_iter().find(|i| i.account == acc(who)).unwrap().state
    }

    #[test]
    fn invites_show_invited_then_accepted_then_expired() {
        // Invitations last 10 minutes in these tests.
        let (mut c, id) = make(true);
        ctx("g1.testnet", MIN);
        assert_eq!(state_of(&c, id, "g1.testnet"), "invited");
        c.accept_guardianship(id);
        assert_eq!(state_of(&c, id, "g1.testnet"), "accepted");
        ctx("g2.testnet", 11 * MIN);
        assert_eq!(state_of(&c, id, "g2.testnet"), "expired");
        // An accepted guardian never expires.
        assert_eq!(state_of(&c, id, "g1.testnet"), "accepted");
    }

    #[test]
    #[should_panic(expected = "invitation has expired")]
    fn an_expired_invite_cannot_be_accepted() {
        let (mut c, id) = make(true);
        ctx("g1.testnet", 11 * MIN);
        c.accept_guardianship(id);
    }

    #[test]
    fn the_owner_can_invite_again_after_expiry() {
        let (mut c, id) = make(true);
        ctx("joseph.testnet", 11 * MIN);
        c.reinvite_guardian(id, acc("g1.testnet"), None);
        assert_eq!(state_of(&c, id, "g1.testnet"), "invited");
        ctx("g1.testnet", 12 * MIN);
        c.accept_guardianship(id);
        assert_eq!(state_of(&c, id, "g1.testnet"), "accepted");
        // g2 was not invited again and is still expired.
        assert_eq!(state_of(&c, id, "g2.testnet"), "expired");
    }

    #[test]
    fn a_pending_guardian_can_be_replaced() {
        let (mut c, id) = make(true);
        ctx("joseph.testnet", MIN);
        c.reinvite_guardian(id, acc("g3.testnet"), Some(acc("g4.testnet")));

        // The invitation lists follow the change.
        assert!(c.get_vaults_for_guardian(acc("g3.testnet")).is_empty());
        assert_eq!(c.get_vaults_for_guardian(acc("g4.testnet")).len(), 1);
        let r = c.get_rules(id);
        assert!(r.guardians.contains(&acc("g4.testnet")));
        assert!(!r.guardians.contains(&acc("g3.testnet")));

        // The vault activates once the new set has accepted.
        for g in ["g1.testnet", "g2.testnet", "g4.testnet"] {
            ctx(g, MIN);
            c.accept_guardianship(id);
        }
        assert_eq!(c.get_status(id).phase, "alive");
    }

    #[test]
    #[should_panic(expected = "not a guardian of this vault")]
    fn a_replaced_guardian_loses_the_invite() {
        let (mut c, id) = make(true);
        ctx("joseph.testnet", MIN);
        c.reinvite_guardian(id, acc("g3.testnet"), Some(acc("g4.testnet")));
        ctx("g3.testnet", MIN);
        c.accept_guardianship(id);
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn only_the_owner_can_invite_again() {
        let (mut c, id) = make(true);
        ctx("g1.testnet", MIN);
        c.reinvite_guardian(id, acc("g2.testnet"), None);
    }

    #[test]
    #[should_panic(expected = "already accepted")]
    fn an_accepted_guardian_cannot_be_replaced() {
        let (mut c, id) = make(true);
        ctx("g1.testnet", MIN);
        c.accept_guardianship(id);
        ctx("joseph.testnet", MIN);
        c.reinvite_guardian(id, acc("g1.testnet"), Some(acc("g4.testnet")));
    }

    #[test]
    #[should_panic(expected = "the owner cannot be a guardian")]
    fn cannot_invite_the_owner_as_a_guardian() {
        let (mut c, id) = make(true);
        ctx("joseph.testnet", MIN);
        c.reinvite_guardian(id, acc("g1.testnet"), Some(acc("joseph.testnet")));
    }

    #[test]
    #[should_panic(expected = "duplicate guardian")]
    fn cannot_invite_the_same_guardian_twice() {
        let (mut c, id) = make(true);
        ctx("joseph.testnet", MIN);
        c.reinvite_guardian(id, acc("g1.testnet"), Some(acc("g2.testnet")));
    }

    // ------------------------------------------------- set up later, rename

    fn owner_ctx(at: u64) {
        ctx_paying("joseph.testnet", at);
    }

    /// A vault with no guardians and no heirs yet.
    fn create_empty(c: &mut InheritanceVault) -> u64 {
        ctx_paying("joseph.testnet", 0);
        c.create_vault(
            "My Family".into(),
            vec![],
            0,
            span(1, Unit::Minutes),
            span(2, Unit::Minutes),
            span(2, Unit::Minutes),
            span(10, Unit::Minutes),
            vec![],
            vec![],
            11_155_111,
        )
    }

    fn two_guardians() -> Vec<AccountId> {
        vec![acc("g1.testnet"), acc("g2.testnet")]
    }

    fn two_heirs() -> Vec<Heir> {
        vec![heir(WIFE, 6000, ""), heir(BROTHER, 4000, "")]
    }

    #[test]
    fn a_vault_can_be_created_with_no_guardians_or_heirs() {
        let mut c = registry();
        let id = create_empty(&mut c);
        let r = c.get_rules(id);
        assert!(r.guardians.is_empty() && r.heirs.is_empty() && !r.setup_complete);
        // The vault is alive from day one, even with nobody to act.
        assert_eq!(c.get_status(id).phase, "alive");
        assert_eq!(c.get_vaults_by_owner(acc("joseph.testnet")).len(), 1);
        // The owner checks in as usual, and a long silence changes nothing:
        // there are no guardians who could act.
        ctx("joseph.testnet", 1000 * MIN);
        assert_eq!(c.get_status(id).phase, "alive");
        c.check_in(id);
        assert_eq!(c.get_status(id).last_check_in_ns, 1000 * MIN);
    }

    #[test]
    fn guardians_and_heirs_can_be_added_later() {
        let mut c = registry();
        let id = create_empty(&mut c);
        owner_ctx(MIN);
        c.set_guardians(id, two_guardians(), 2);
        // Guardians are named but have not accepted yet.
        assert_eq!(c.get_status(id).phase, "inactive");
        assert!(!c.get_rules(id).setup_complete); // still no heirs
        assert_eq!(c.get_vaults_for_guardian(acc("g1.testnet")).len(), 1);
        owner_ctx(MIN);
        c.set_heirs(id, two_heirs(), vec![]);
        assert!(c.get_rules(id).setup_complete);
        assert_eq!(c.get_status(id).phase, "inactive"); // waiting for guardians
        for g in ["g1.testnet", "g2.testnet"] {
            ctx(g, MIN);
            c.accept_guardianship(id);
        }
        assert_eq!(c.get_status(id).phase, "alive");
    }

    #[test]
    fn finishing_the_setup_restarts_the_silence_clock() {
        // Created at 0 but only finished an hour later: guardians must not be
        // able to act immediately.
        let mut c = registry();
        let id = create_empty(&mut c);
        owner_ctx(60 * MIN);
        c.set_guardians(id, two_guardians(), 2);
        owner_ctx(60 * MIN);
        c.set_heirs(id, two_heirs(), vec![]);
        for g in ["g1.testnet", "g2.testnet"] {
            ctx(g, 60 * MIN);
            c.accept_guardianship(id);
        }
        let st = c.get_status(id);
        assert_eq!(st.phase, "alive");
        assert_eq!(st.last_check_in_ns, 60 * MIN);
    }

    #[test]
    fn changing_the_setup_counts_as_a_check_in_but_renaming_does_not() {
        let mut c = registry();
        let id = create_empty(&mut c);
        owner_ctx(100 * MIN);
        c.rename_vault(id, "Renamed".into());
        assert_eq!(c.get_status(id).last_check_in_ns, 0);
        owner_ctx(200 * MIN);
        c.set_guardians(id, two_guardians(), 2);
        assert_eq!(c.get_status(id).last_check_in_ns, 200 * MIN);
        owner_ctx(300 * MIN);
        c.set_heirs(id, two_heirs(), vec![]);
        assert_eq!(c.get_status(id).last_check_in_ns, 300 * MIN);
    }

    #[test]
    #[should_panic(expected = "setup is not finished")]
    fn guardians_cannot_approve_without_heirs() {
        // Guardians named and accepted, but no heirs: nothing could be paid.
        let mut c = registry();
        let id = create_empty(&mut c);
        owner_ctx(MIN);
        c.set_guardians(id, two_guardians(), 2);
        for g in ["g1.testnet", "g2.testnet"] {
            ctx(g, MIN);
            c.accept_guardianship(id);
        }
        assert_eq!(c.get_status(id).phase, "alive");
        ctx("g1.testnet", 60 * MIN);
        c.approve_release(id);
    }

    #[test]
    #[should_panic(expected = "setup is not finished")]
    fn guardians_cannot_approve_an_unfinished_vault() {
        let mut c = registry();
        let id = create_empty(&mut c);
        ctx("g1.testnet", 60 * MIN);
        c.approve_release(id);
    }

    #[test]
    fn changing_guardians_keeps_accepted_ones_and_updates_lists() {
        let (mut c, id) = make(true); // g1, g2, g3
        ctx("g1.testnet", MIN);
        c.accept_guardianship(id);
        owner_ctx(2 * MIN);
        c.set_guardians(id, vec![acc("g1.testnet"), acc("g4.testnet")], 2);

        let r = c.get_rules(id);
        assert_eq!(r.guardians, vec![acc("g1.testnet"), acc("g4.testnet")]);
        assert_eq!(r.guardians_accepted, vec![acc("g1.testnet")]);
        assert!(c.get_vaults_for_guardian(acc("g2.testnet")).is_empty());
        assert_eq!(c.get_vaults_for_guardian(acc("g1.testnet")).len(), 1);
        assert_eq!(c.get_vaults_for_guardian(acc("g4.testnet")).len(), 1);
        // The new guardian's invitation starts now: 2 min + 10 min validity.
        let inv = r.guardian_invites.iter().find(|i| i.account == acc("g4.testnet")).unwrap();
        assert_eq!(inv.expires_at_ns, 12 * MIN);
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn only_the_owner_can_change_guardians() {
        let (mut c, id) = make(true);
        ctx_paying("g1.testnet", MIN);
        c.set_guardians(id, two_guardians(), 2);
    }

    #[test]
    #[should_panic(expected = "the owner cannot be a guardian")]
    fn guardians_cannot_include_the_owner() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.set_guardians(id, vec![acc("joseph.testnet"), acc("g2.testnet")], 2);
    }

    #[test]
    #[should_panic(expected = "duplicate guardian")]
    fn guardians_cannot_repeat() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.set_guardians(id, vec![acc("g1.testnet"), acc("g1.testnet")], 1);
    }

    #[test]
    #[should_panic(expected = "threshold must be between")]
    fn threshold_must_fit_the_guardians() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.set_guardians(id, vec![acc("g1.testnet")], 2);
    }

    #[test]
    #[should_panic(expected = "while a release is pending")]
    fn guardians_cannot_change_during_a_release() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        owner_ctx(3 * MIN);
        c.set_guardians(id, two_guardians(), 2);
    }

    #[test]
    fn the_owner_can_replace_the_heirs() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.set_heirs(id, vec![heir(WIFE, 10000, "")], vec![]);
        let r = c.get_rules(id);
        assert_eq!(r.heirs.len(), 1);
        assert!(!r.heirs_hidden);
    }

    #[test]
    fn heirs_can_be_switched_to_hidden() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        let commitments = vec![commitment(&heir(WIFE, 6000, "s1")), commitment(&heir(BROTHER, 4000, "s2"))];
        c.set_heirs(id, vec![], commitments);
        let r = c.get_rules(id);
        assert!(r.heirs_hidden && r.heirs.is_empty() && r.heir_commitments.len() == 2);
    }

    #[test]
    #[should_panic(expected = "add up to exactly 10000")]
    fn new_heirs_must_total_100_percent() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.set_heirs(id, vec![heir(WIFE, 5000, "")], vec![]);
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn only_the_owner_can_change_heirs() {
        let (mut c, id) = make(true);
        ctx_paying("g1.testnet", MIN);
        c.set_heirs(id, two_heirs(), vec![]);
    }

    #[test]
    #[should_panic(expected = "while a release is pending")]
    fn heirs_cannot_change_during_a_release() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        owner_ctx(3 * MIN);
        c.set_heirs(id, two_heirs(), vec![]);
    }

    #[test]
    fn the_owner_can_rename_the_vault() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.rename_vault(id, "Home".into());
        assert_eq!(c.get_rules(id).name, "Home");
        assert_eq!(c.get_vaults_by_owner(acc("joseph.testnet"))[0].name, "Home");
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn only_the_owner_can_rename_the_vault() {
        let (mut c, id) = make(true);
        ctx_paying("g1.testnet", MIN);
        c.rename_vault(id, "Mine now".into());
    }

    #[test]
    #[should_panic(expected = "name must be")]
    fn the_vault_name_cannot_be_empty() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.rename_vault(id, "   ".into());
    }

    #[test]
    fn heirs_can_be_given_names_and_renamed() {
        let mut c = registry();
        let id = create_empty(&mut c);
        owner_ctx(MIN);
        c.set_heirs(id, vec![named(WIFE, 6000, "Anna"), named(BROTHER, 4000, "")], vec![]);
        assert_eq!(c.get_rules(id).heirs[0].label, "Anna");

        owner_ctx(2 * MIN);
        c.rename_heir(id, BROTHER.to_string(), "  Ben  ".into());
        owner_ctx(3 * MIN);
        c.rename_heir(id, WIFE.to_string(), "Anna K.".into());
        let r = c.get_rules(id);
        assert_eq!(r.heirs[0].label, "Anna K.");
        assert_eq!(r.heirs[1].label, "Ben"); // trimmed
        // Names never change who is paid or how much.
        assert_eq!(r.heirs[0].eth_address, WIFE);
        assert_eq!(r.heirs[0].bps, 6000);
    }

    #[test]
    #[should_panic(expected = "no heir with that address")]
    fn cannot_name_an_address_that_is_not_an_heir() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.rename_heir(id, "0x3333333333333333333333333333333333333333".into(), "Eve".into());
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn only_the_owner_can_rename_an_heir() {
        let (mut c, id) = make(true);
        ctx_paying("g1.testnet", MIN);
        c.rename_heir(id, WIFE.to_string(), "Hacked".into());
    }

    #[test]
    #[should_panic(expected = "at most 64 characters")]
    fn heir_names_have_a_length_limit() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.rename_heir(id, WIFE.to_string(), "x".repeat(65));
    }

    #[test]
    fn names_sent_with_a_reveal_are_dropped() {
        let (mut c, id) = make(false);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("anyone.testnet", 3 * MIN);
        // The revealer tries to attach text; it is dropped.
        let mut wife = heir(WIFE, 6000, "s1");
        let mut brother = heir(BROTHER, 4000, "s2");
        wife.label = "evil".into();
        brother.label = "evil".into();
        c.reveal_heirs(id, vec![wife, brother]);
        let r = c.get_rules(id);
        assert_eq!(r.heirs.len(), 2);
        assert!(r.heirs.iter().all(|h| h.label.is_empty()));
    }

    #[test]
    fn heirs_can_be_updated_share_address_and_name() {
        const NEW_ADDR: &str = "0x4444444444444444444444444444444444444444";
        let mut c = registry();
        let id = create_empty(&mut c);
        owner_ctx(MIN);
        c.set_heirs(id, vec![named(WIFE, 6000, "Anna"), named(BROTHER, 4000, "Ben")], vec![]);

        // Change shares only: names stay.
        owner_ctx(2 * MIN);
        c.set_heirs(id, vec![heir(WIFE, 7000, ""), heir(BROTHER, 3000, "")], vec![]);
        let r = c.get_rules(id);
        assert_eq!((r.heirs[0].bps, r.heirs[1].bps), (7000, 3000));
        assert_eq!((r.heirs[0].label.as_str(), r.heirs[1].label.as_str()), ("Anna", "Ben"));

        // Ben moves to a new address. It is a different address, so it is a
        // fresh entry: send his name again if you want to keep it.
        owner_ctx(3 * MIN);
        c.set_heirs(id, vec![heir(WIFE, 7000, ""), named(NEW_ADDR, 3000, "Ben")], vec![]);
        let r = c.get_rules(id);
        assert_eq!(r.heirs[1].eth_address, NEW_ADDR);
        assert_eq!(r.heirs[1].label, "Ben");

        // Name-only change, any time.
        owner_ctx(4 * MIN);
        c.rename_heir(id, NEW_ADDR.to_string(), "Benjamin".into());
        assert_eq!(c.get_rules(id).heirs[1].label, "Benjamin");
    }

    /// A vault past its countdown, with the numbers fixed.
    fn started_release() -> (InheritanceVault, u64) {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g2.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g1.testnet", 6 * MIN);
        c.start_execution(id, 5, U128(1_000_000_000_000_000_000), U128(1_000_000_000), U128(100_000_000));
        (c, id)
    }

    /// A vault past its countdown, before the numbers are fixed.
    fn ready_to_release() -> (InheritanceVault, u64) {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g2.testnet", 3 * MIN);
        c.approve_release(id);
        (c, id)
    }

    #[test]
    #[should_panic(expected = "only a guardian can carry out the release")]
    fn the_owner_cannot_start_the_release() {
        let (mut c, id) = ready_to_release();
        ctx("joseph.testnet", 6 * MIN);
        c.start_execution(id, 0, U128(1_000_000_000_000_000_000), U128(1_000_000_000), U128(100_000_000));
    }

    #[test]
    #[should_panic(expected = "only a guardian can carry out the release")]
    fn a_stranger_cannot_start_the_release() {
        let (mut c, id) = ready_to_release();
        ctx("stranger.testnet", 6 * MIN);
        c.start_execution(id, 0, U128(1_000_000_000_000_000_000), U128(1_000_000_000), U128(100_000_000));
    }

    #[test]
    #[should_panic(expected = "only a guardian can carry out the release")]
    fn the_owner_cannot_sign_a_payout() {
        let (c, id) = started_release();
        ctx("joseph.testnet", 7 * MIN);
        c.vault(id).build_payout(0);
    }

    #[test]
    fn a_failed_release_can_be_reset_and_run_again() {
        let (mut c, id) = started_release();
        assert_eq!(c.get_status(id).execution_base_nonce, Some(5));
        ctx("g1.testnet", 7 * MIN);
        c.reset_execution(id);
        let st = c.get_status(id);
        assert!(!st.execution_started);
        assert_eq!(st.execution_base_nonce, None);
        // The heirs are unchanged, and the numbers can be fixed again.
        c.start_execution(id, 6, U128(900_000_000_000_000_000), U128(1_000_000_000), U128(100_000_000));
        assert_eq!(c.get_status(id).execution_base_nonce, Some(6));
        assert_eq!(c.get_rules(id).heirs.len(), 2);
    }

    #[test]
    #[should_panic(expected = "only a guardian can carry out the release")]
    fn the_owner_cannot_reset_a_release() {
        let (mut c, id) = started_release();
        ctx("joseph.testnet", 7 * MIN);
        c.reset_execution(id);
    }

    #[test]
    #[should_panic(expected = "only a guardian can carry out the release")]
    fn a_stranger_cannot_reset_a_release() {
        let (mut c, id) = started_release();
        ctx("stranger.testnet", 7 * MIN);
        c.reset_execution(id);
    }

    #[test]
    #[should_panic(expected = "execution has not started")]
    fn reset_needs_a_started_release() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g2.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g1.testnet", 6 * MIN);
        c.reset_execution(id);
    }

    #[test]
    fn the_owner_can_restart_a_released_vault() {
        let (mut c, id) = started_release();
        ctx("joseph.testnet", 20 * MIN);
        c.restart_vault(id);
        let st = c.get_status(id);
        assert_eq!(st.phase, "alive");
        assert!(st.approvals.is_empty() && !st.execution_started);
        assert_eq!(st.last_check_in_ns, 20 * MIN);
        // Normal life again: check-ins work, and guardians and heirs are unchanged.
        ctx("joseph.testnet", 21 * MIN);
        c.check_in(id);
        let r = c.get_rules(id);
        assert_eq!((r.guardians.len(), r.heirs.len()), (3, 2));
        assert_eq!(r.guardians_accepted.len(), 3);
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn a_guardian_cannot_restart_a_vault() {
        let (mut c, id) = started_release();
        ctx("g1.testnet", 20 * MIN);
        c.restart_vault(id);
    }

    #[test]
    #[should_panic(expected = "the release has not started")]
    fn restart_is_not_a_late_cancel() {
        // Countdown over, but the release has not started: the owner still cannot stop it.
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g2.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("joseph.testnet", 6 * MIN);
        c.restart_vault(id);
    }

    #[test]
    #[should_panic(expected = "the countdown has not finished")]
    fn restart_does_not_work_during_the_countdown() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("g2.testnet", 3 * MIN);
        c.approve_release(id);
        ctx("joseph.testnet", 4 * MIN);
        c.restart_vault(id);
    }

    #[test]
    fn the_gas_limit_covers_a_brand_new_address() {
        // Sepolia needed 207,391 gas for a transfer to an unused address.
        assert!(TRANSFER_GAS_LIMIT >= 207_391);
    }

    const MOVED: &str = "0x5555555555555555555555555555555555555555";

    #[test]
    fn an_heirs_address_can_be_updated_in_place() {
        let mut c = registry();
        let id = create_empty(&mut c);
        owner_ctx(MIN);
        c.set_heirs(id, vec![named(WIFE, 6000, "Anna"), named(BROTHER, 4000, "Ben")], vec![]);
        owner_ctx(2 * MIN);
        c.update_heir_address(id, BROTHER.to_string(), MOVED.to_string());
        let r = c.get_rules(id);
        // Same position, share and name; only the address changed.
        assert_eq!(r.heirs[1].eth_address, MOVED);
        assert_eq!(r.heirs[1].bps, 4000);
        assert_eq!(r.heirs[1].label, "Ben");
        assert_eq!(r.heirs[0].eth_address, WIFE);
    }

    #[test]
    #[should_panic(expected = "duplicate heir address")]
    fn an_address_cannot_move_onto_another_heir() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.update_heir_address(id, BROTHER.to_string(), WIFE.to_string());
    }

    #[test]
    #[should_panic(expected = "invalid Ethereum address")]
    fn the_new_address_must_be_valid() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.update_heir_address(id, BROTHER.to_string(), "0x123".to_string());
    }

    #[test]
    #[should_panic(expected = "no heir with that address")]
    fn only_an_existing_heir_address_can_be_updated() {
        let (mut c, id) = make(true);
        owner_ctx(MIN);
        c.update_heir_address(id, MOVED.to_string(), "0x6666666666666666666666666666666666666666".to_string());
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn only_the_owner_can_update_an_address() {
        let (mut c, id) = make(true);
        ctx_paying("g1.testnet", MIN);
        c.update_heir_address(id, BROTHER.to_string(), MOVED.to_string());
    }

    #[test]
    #[should_panic(expected = "while a release is pending")]
    fn an_address_cannot_change_during_a_release() {
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        owner_ctx(3 * MIN);
        c.update_heir_address(id, BROTHER.to_string(), MOVED.to_string());
    }

    #[test]
    fn an_heir_can_be_renamed_during_a_release() {
        // Names never affect payouts, so renaming stays possible.
        let (mut c, id) = make(true);
        activate(&mut c, id);
        ctx("g1.testnet", 3 * MIN);
        c.approve_release(id);
        owner_ctx(3 * MIN);
        c.rename_heir(id, WIFE.to_string(), "Anna".into());
        assert_eq!(c.get_rules(id).heirs[0].label, "Anna");
    }

    #[test]
    fn heirs_can_be_added_and_removed() {
        const CARA: &str = "0x3333333333333333333333333333333333333333";
        let mut c = registry();
        let id = create_empty(&mut c);
        owner_ctx(MIN);
        c.set_heirs(id, vec![named(WIFE, 6000, "Anna"), named(BROTHER, 4000, "Ben")], vec![]);
        assert_eq!(c.get_rules(id).heirs.len(), 2);

        // Remove Ben: Anna takes the whole share. Her name is kept.
        owner_ctx(2 * MIN);
        c.set_heirs(id, vec![heir(WIFE, 10000, "")], vec![]);
        let r = c.get_rules(id);
        assert_eq!(r.heirs.len(), 1);
        assert_eq!(r.heirs[0].label, "Anna");
        assert_eq!(r.heirs[0].bps, 10000);

        // Add Ben again, and Cara: three heirs, shares re-split.
        owner_ctx(3 * MIN);
        c.set_heirs(
            id,
            vec![heir(WIFE, 5000, ""), named(BROTHER, 3000, "Benjamin"), named(CARA, 2000, "Cara")],
            vec![],
        );
        let r = c.get_rules(id);
        assert_eq!(r.heirs.len(), 3);
        let names: Vec<&str> = r.heirs.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(names, vec!["Anna", "Benjamin", "Cara"]);
        assert_eq!(r.heirs.iter().map(|h| h.bps as u32).sum::<u32>(), 10_000);

        // Removing everyone is allowed too: the vault just has nobody to pay yet.
        owner_ctx(4 * MIN);
        c.set_heirs(id, vec![], vec![]);
        assert!(c.get_rules(id).heirs.is_empty());
        assert!(!c.get_rules(id).setup_complete);
    }
}
