# Pal: inheritance vaults on NEAR + Ethereum (testnet only)

A NEAR contract that controls Ethereum addresses through Chain Signatures and releases them to named heirs if the owner goes silent. Web app in [`../frontend`](../frontend). **Testnet only: NEAR testnet and Ethereum Sepolia. No real money.**

**One idea:** guardians decide *when*, never *who*. The heirs and their shares are set by the owner. Guardians can only start a countdown, and the owner can cancel it until it ends.

## How it works

One contract is deployed once. It holds **many vaults** ("My Family", "Workplace"). Each vault is separate: its own owner, guardians, heirs, timers and its own Ethereum address.

1. **Create a vault** (`create_vault`). The caller becomes the owner. Guardians and heirs are optional: a vault can be created empty and filled in later.
2. **Set up.** The owner adds guardians (`set_guardians`) and heirs (`set_heirs`), and can rename the vault or an heir, or move an heir to a new address. Guardians get an invitation that expires; the owner can invite again or replace a guardian who has not accepted.
3. **Guardians accept** (`accept_guardianship`). Until they all have, the vault shows `inactive`, and guardians cannot act.
4. **Fund it.** Each vault has its own Ethereum address (derived from the contract account and the path `vault-<id>`). Send Sepolia test ETH to it. The ETH never enters the NEAR contract.
5. **Check in** (`check_in`). It resets the silence clock and cancels any pending release. Changing the guardians or heirs also counts as a check-in.
6. **Silence.** After the silence period, guardians may call `approve_release`.
7. **Countdown.** When enough guardians have approved, the waiting period starts. The owner can `cancel_release` or `check_in` any time before it ends.
8. **Release.** After the countdown a guardian presses **Release funds**, and the owner cannot. The app runs `reveal_heirs` (if hidden), `start_execution` (fixes balance, nonce and fees), then `sign_payout` once per heir, and broadcasts each payment. The contract builds each Ethereum transfer itself, so the destination and the amount come from the stored rules, not from the caller. A contract cannot start anything by itself, so a guardian must press the button: fully automatic release would need an always-on server. If every guardian loses access, the funds stay in the vault.
9. **After a release.** If a payment failed, a guardian can retry (`reset_execution`). With several heirs, check who was paid first. Once the heirs are paid, the owner can press **Start again** (`restart_vault`) to return the vault to normal. It works only after the release has started, so it cannot be used as a late cancel. The contract cannot see Ethereum, so it cannot confirm the payment happened: this is a known weak point.

### Timers

| Setting | Meaning | Example | Demo mode |
|---|---|---|---|
| Check-in interval | How often the owner should check in | 30 days | 1 minute |
| Silence period | Silence before guardians may act (at least the interval) | 90 days | 2 minutes |
| Waiting period | Countdown after approval; the owner can cancel until it ends | 14 days | 2 minutes |
| Invitation validity | How long a guardian invitation stays valid | 7 days | 10 minutes |

Units are `Minutes | Hours | Days | Months | Years`. A month is 30 days and a year is 365 days. Maximum 100 years.

### Phases (`get_status`)

`inactive` (guardians named but not all accepted) · `alive` · `guardians_may_act` · `pending_approval` · `countdown` · `executable`.
A vault with no guardians or heirs yet is `alive`, with `setup_complete: false`; guardians cannot approve a release until it has at least one guardian and one heir.

### Who can see a vault

In the web app, a vault appears only to its owner and its guardians. This is a screen rule, not a privacy guarantee: the contract's data is public on the blockchain, so anyone can read names, guardians and public heirs directly. Use hidden heirs to keep heirs off the chain. Vault names should not contain anything private.

### Hidden heirs (optional)

Instead of listing heir addresses, the owner stores `sha256("lowercase_address|bps|salt")` for each. Heirs become visible only after a release is pending, when someone holding the secret codes calls `reveal_heirs`. Every revealed heir must match a stored hash. If a secret code is lost, that heir cannot be revealed. Names of hidden heirs are never stored.

### What the owner can change, and when

The owner can change guardians and heirs until a release starts (`pending_approval`, `countdown` or `executable`); then they are locked. Renaming the vault or an heir is always allowed. **This is weaker than "fixed at setup":** an owner who can change the heirs can do so until a release begins, and anyone reading the rules sees the current list.

## What is verified, and what is not

**Verified by running it:**
- 78 contract unit tests pass.
- The wasm builds (241 KB) and is deployed on testnet at `family-vault.josep-vault.testnet`, checksum matching the build.
- `create_vault` with the exact JSON the web app produces (public heirs with names, guardians, demo timers) works on testnet and reads back correctly.
- The full guardian life cycle, **12 steps on a real vault with minute timers** (see [CONTRACT_GUIDE.md](CONTRACT_GUIDE.md)): early approvals refused, approvals counted, countdown, owner cancel, check-in clearing a release, and cancel/check-in refused once the countdown has ended.
- The MPC signer's testnet interface (`payload_v2`, `domain_id`, 1 yoctoNEAR deposit accepted) and its answer format.
- Signing maths end to end against the live signer: a transaction hash was signed, turned into a valid Ethereum transaction, and the signature recovers to exactly the address `derived_public_key` assigns to that path.
- Vault addresses derived in the browser match the ones computed separately.
- The hidden-heir fingerprint computed in the browser equals the contract's `compute_commitment`.
- The app builds and type-checks; a vault link opened without signing in shows only a sign-in prompt (headless browser).
- Login with email through Privy works (sign-in, NEAR account created, balance read). That test used the earlier NEAR Connect route; the app has since moved to the Direct Privy route.
- **A full release end to end on testnet (vault 4):** real Privy guardians accepted and approved, the countdown ran, the contract built the payment, the MPC network signed it, and 0.0496979 Sepolia ETH reached the heir. The first attempt failed with "Out of gas" because a transfer to a brand-new address needs about 207,000 gas, so the limit is now 300,000.
- Real Privy signing in a browser: a vault created from the wizard (vault 4) exists on testnet, owned by the Privy account, with the unused storage deposit refunded.
- The app's Direct Privy signer on testnet, with Privy's signing replaced by a throwaway key: `create_vault` with a deposit, a method with no return value, and a contract refusal coming through as plain text.
- A person clicking through the web app on testnet with real Privy logins: creating a vault, guardians accepting and approving, a guardian releasing funds, revealing a hidden heir, the owner cancelling, and the owner pressing **Start again**.

**Not verified yet:**
- The signed-in screens on a real phone.
- The "Export my private key" button for NEAR wallets.
- Login with Google. External NEAR wallets are not supported in version 1 (Direct Privy route).
- Guardian invitations by email (not built: needs a small backend and Privy's server API).
- The storage deposit sizing for large vaults (unit tests cannot measure storage).
- Mainnet: not supported and not intended.
- `near-sdk` 5.30.0 and Privy are recent releases; the dependency audit reports vulnerabilities inside Privy's own dependency tree, not yet reviewed.

## Failure cases (honest list)

**Lost guardian keys.** With 2 of 3, one lost key changes nothing; two lost keys mean a release can never start, but funds stay safe. Guardians who have not accepted can be replaced by the owner; guardians who have accepted can be removed by the owner while no release is under way.

**A guardian turns hostile.** One guardian alone can do nothing. Even a hostile majority can only start the countdown; they cannot change the heirs or redirect funds, and the owner can cancel during the countdown.

**The owner is alive but offline.** The weakest case. If the owner cannot check in or cancel for longer than silence plus waiting, the vault releases even though they are alive. Use long periods and several ways to remind the owner.

**No recovery key in version 1.** If the owner loses their login (their Privy account), they cannot check in or cancel.

**Wrong balance or nonce at execution.** A NEAR contract cannot read Ethereum, so the caller supplies the balance, nonce and fees to `start_execution`, and they are fixed once. The app reads them just before and shows them first, but a wrong value could produce a payment that fails.

**Dependence on the MPC network.** If the signer network is unavailable, `sign_payout` fails and can be retried.

**Dependence on Privy.** Email and Google logins hold the NEAR key in Privy's infrastructure.

**Rounding.** Payouts round down; the few leftover wei stay in the vault.

## Layout

```
inheritance-vault/contract/   the contract (Rust, near-sdk 5.30.0) and its tests
inheritance-vault/CONTRACT_GUIDE.md   every command that was run
../frontend/                  the web app (Vite, React, TypeScript, Privy + near-api-js)
```

## Run it

```powershell
# contract tests
cd contract; $env:AWS_LC_SYS_PREBUILT_NASM = "1"; cargo test

# web app
cd ..\..\frontend
npm install
# fill in .env.local (Privy App ID and Client ID, the contract account); see .env.example
npm run dev        # http://localhost:5173
```

## Deploy (testnet only)

The contract is one account that holds every vault, so it is deployed once and never per vault. Run these from this folder. Full setup and account creation are in [CONTRACT_GUIDE.md](CONTRACT_GUIDE.md).

```powershell
# build: writes contract\target\near\inheritance_vault.wasm
cd contract; $env:AWS_LC_SYS_PREBUILT_NASM = "1"; cargo near build non-reproducible-wasm --locked --no-abi; cd ..

# first deploy on a new account (init-args.json contains {"signer":"v1.signer-prod.testnet"})
near contract deploy <account>.testnet use-file contract\target\near\inheritance_vault.wasm with-init-call new file-args init-args.json prepaid-gas '30 Tgas' attached-deposit '0 NEAR' network-config testnet sign-with-keychain send

# replace the code on the same account (keeps all stored vaults)
near contract deploy <account>.testnet use-file contract\target\near\inheritance_vault.wasm without-init-call network-config testnet sign-with-keychain send
```

- Only use the second command if the stored data has not changed shape. Changing the fields of a stored struct breaks reading the existing vaults.
- Each Ethereum address comes from the contract account and the vault's path. A new contract account means new addresses, so a vault on the old account stays there.
- Set `VITE_VAULT_ACCOUNT` in `frontend/.env.local` to the contract account.

## Demo script (about ten minutes, minute timers)

1. Sign in with email. Open the account menu and make sure the account has test NEAR.
2. **New vault**, name it, add two heirs (60% / 40%), add guardians, turn on **Demo mode**, create it.
3. Each guardian signs in with their own login and presses **Accept guardianship** on the vault page.
4. Send Sepolia test ETH to the vault's address (shown on the vault page).
5. The owner checks in, then stays silent for two minutes.
6. Two guardians press **Approve release**. The countdown starts.
7. Run A: wait two minutes, then a guardian presses **Release funds** on the Overview page. Run B: during the countdown the owner presses **Cancel the release**; after the countdown ends the same button is gone and the page says it can no longer be cancelled.
