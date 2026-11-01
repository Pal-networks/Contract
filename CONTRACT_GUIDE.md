# Contract guide (Windows, PowerShell, NEAR testnet)

Every command below was actually run, in this order, while building and testing the contract. Nothing here touches mainnet.
Account names are the ones used in this project; replace them with yours.

| Account | Role |
|---|---|
| `josep-vault.testnet` | The owner / payer. Created by the CLI faucet (10 NEAR). |
| `family-vault.josep-vault.testnet` | The contract. Deployed once. Holds every vault. |
| `g1`, `g2`, `g3` `.josep-vault.testnet` | Test guardians for command-line testing. |
| `pal-funds.testnet` | A spare faucet-funded account used to top up Privy logins. |

## 1. Install the tools

Needed once. The Rust toolchain on Windows needs Microsoft's C++ build tools (Visual Studio, "Desktop development with C++"); without them `cargo` fails with `linker link.exe not found`.

```powershell
rustup update stable                      # Rust 1.99.1 was current; near-sdk 5.30 needs 1.95 or newer
rustup target add wasm32-unknown-unknown
irm https://github.com/near/cargo-near/releases/latest/download/cargo-near-installer.ps1 | iex
irm https://github.com/near/near-cli-rs/releases/latest/download/near-cli-rs-installer.ps1 | iex
rustc --version; cargo near --version; near --version      # open a NEW terminal first
```

Do not use `cargo install cargo-near` on an older Rust: it refuses ("requires rustc 1.93 or newer"). The prebuilt installers above avoid that.

## 2. Test (66 unit tests)

```powershell
cd contract
$env:AWS_LC_SYS_PREBUILT_NASM = "1"       # needed on Windows: the test libraries want NASM; this uses prebuilt files instead
cargo test
```

Unit tests run on a fake blockchain. They do not prove gas, storage payment or the live signer; the testnet steps below do.

## 3. Build the wasm

```powershell
cargo near build non-reproducible-wasm --locked --no-abi
# -> contract\target\near\inheritance_vault.wasm  (about 238 KB)
```

`--no-abi` is needed on Windows: the ABI step builds a native library, and near-sdk 5.30 cannot link that on Windows. The ABI is only a description of the methods; the contract does not need it.
A plain `cargo near build` with an older Rust stops with "wasm compiled with rustc 1.88 exceeds the max allowed 1.86". Update Rust, or use a near-sdk that declares the newer protocol (5.30.0 does).

## 4. Create the accounts

```powershell
# Owner, funded by the faucet. The key goes to the Windows keychain; nothing secret is printed.
near account create-account sponsor-by-faucet-service josep-vault.testnet autogenerate-new-keypair save-to-keychain network-config testnet create

# Contract account and test guardians, paid by the owner
near account create-account fund-myself family-vault.josep-vault.testnet '4 NEAR' autogenerate-new-keypair save-to-keychain sign-as josep-vault.testnet network-config testnet sign-with-keychain send
near account create-account fund-myself g1.josep-vault.testnet '0.5 NEAR' autogenerate-new-keypair save-to-keychain sign-as josep-vault.testnet network-config testnet sign-with-keychain send
# ... same for g2 and g3
```

Check a name is free first: `near account view-account-summary NAME network-config testnet now` (an error "does not exist" means it is free).

## 5. Deploy and initialise (once)

```powershell
# init-args.json contains: {"signer":"v1.signer-prod.testnet"}
near contract deploy family-vault.josep-vault.testnet use-file contract\target\near\inheritance_vault.wasm with-init-call new file-args init-args.json prepaid-gas '30 Tgas' attached-deposit '0 NEAR' network-config testnet sign-with-keychain send
```

To replace the code later (keeps all stored vaults), skip the init call:

```powershell
near contract deploy family-vault.josep-vault.testnet use-file contract\target\near\inheritance_vault.wasm without-init-call network-config testnet sign-with-keychain send
```

Verify: `near account view-account-summary family-vault.josep-vault.testnet network-config testnet now` shows the contract's SHA-256, which must equal the wasm you built.

## 6. Calling the contract

Pass arguments from a **file** (`file-args`). Inline JSON gets mangled by PowerShell quoting.

```powershell
# read (free, no signer)
near contract call-function as-read-only family-vault.josep-vault.testnet get_rules file-args vault1.json network-config testnet now

# write
near contract call-function as-transaction family-vault.josep-vault.testnet create_vault file-args create-args.json prepaid-gas '50 Tgas' attached-deposit '0.1 NEAR' sign-as josep-vault.testnet network-config testnet sign-with-keychain send
```

Methods: `new`, `create_vault`, `rename_vault`, `set_guardians`, `reinvite_guardian`, `accept_guardianship`, `set_heirs`, `update_heir_address`, `rename_heir`, `check_in`, `cancel_release`, `approve_release`, `reveal_heirs`, `start_execution`, `sign_payout`, and the reads `get_rules`, `get_status`, `get_vaults_by_owner`, `get_vaults_for_guardian`, `get_vault_count`, `get_signer`, `compute_commitment`.
Editing methods take a small NEAR deposit for storage (0.05 to 0.1); what is not used is refunded.

## 7. The MPC signer (checked against the live testnet contract)

```powershell
near contract inspect v1.signer-prod.testnet network-config testnet now
near contract download-abi v1.signer-prod.testnet save-to-file signer-abi.json network-config testnet now
```

- `sign` takes `{"request": {"payload_v2": {"Ecdsa": "<64 hex>"}, "path": "...", "domain_id": 0}}`. Domain 0 is the Secp256k1 key. The old `payload` / `key_version` form is still accepted but deprecated; the contract sends the new form.
- A deposit of **1 yoctoNEAR** was accepted. The signer needs at least 15 Tgas attached to `sign`; `sign_payout` attaches 250 Tgas for it.
- The signer returns `{"big_r": {"affine_point"}, "s": {"scalar"}, "recovery_id", "scheme": "Secp256k1"}`.
- A vault's Ethereum address comes from `derived_public_key(path="vault-<id>", predecessor=<contract account>, domain_id=0)`: Keccak-256 of the 64-byte key, last 20 bytes.

## 8. The manual test script on a real vault (vault 2, timers 1 / 2 / 2 minutes)

Run against testnet with `g1`, `g2`, `g3` signing from the command line.

| # | Action | Result |
|---|---|---|
| 1 | `get_status` | `inactive` — pass |
| 2 | Non-guardian calls `accept_guardianship` | rejected: "you are not a guardian of this vault" — pass |
| 3 | `g1`, `g2`, `g3` accept | status `alive` — pass |
| 4 | `g1` approves at once | rejected: "the owner has not been silent long enough" — pass |
| 5 | Wait the silence period | `guardians_may_act` — pass |
| 6 | `g1` approves | `pending_approval` — pass |
| 7 | `g2` approves | `countdown` — pass |
| 8 | `g3` (a guardian) cancels | rejected: "only the owner can do this" — pass |
| 9 | Owner cancels | back to `guardians_may_act` — pass |
| 10 | Approve twice, then owner checks in | release cleared, `alive` — pass |
| 11 | Silence, approve twice, wait the countdown | `executable` — pass |
| 12 | Owner cancels or checks in | rejected: "the waiting period has ended; the release can no longer be cancelled" — pass |

(My first run of steps 10 to 12 reported failures. That was a bug in my test script: PowerShell treats `$c` and `$C` as the same variable. After fixing it, steps 10 to 12 passed.)

## 9. If it fails

| What you see | What it means | Fix |
|---|---|---|
| `linker link.exe not found` | No Microsoft C++ build tools | Install Visual Studio with "Desktop development with C++" |
| `cannot install package cargo-near … requires rustc 1.93` | Rust is too old for building cargo-near from source | Use the prebuilt installer, or update Rust |
| `wasm … exceeds the max allowed 1.86.0` | Rust too new for the protocol the SDK declares | Use near-sdk 5.30.0 (declares a newer protocol) |
| `failed to run custom build command for aws-lc-sys` / `NASM command not found` | Test libraries need NASM on Windows | `$env:AWS_LC_SYS_PREBUILT_NASM = "1"` |
| Many `LNK2019 unresolved external symbol` during `cargo near build` | The ABI step links a native library | Add `--no-abi` |
| `Failed to deserialize input from JSON … expected value at line 1 column 1` | A hidden byte-order mark at the start of the args file (PowerShell 5.1 `-Encoding utf8` adds one) | Write the file without BOM (`[System.IO.File]::WriteAllText(path, text, (New-Object System.Text.UTF8Encoding($false)))`) |
| `account … does not exist while viewing` | The account is not created yet | Create it, or send it NEAR first (implicit accounts exist only after they receive NEAR) |
| `send-near` hangs | The CLI shows a yes/no menu for a receiver that does not exist yet; scripts cannot answer it | Use `near transaction construct-transaction … add-action transfer '1 NEAR' skip …` |
| `Smart contract panicked: <text>` | The contract refused; the text is its own message | Read the message |
| Command stuck with no output | The CLI is waiting for a typed answer | Run it with stdin closed, or give every argument on the command line |
| The faucet website says "Invalid account" for a 64-character account | The faucet's own address check rejects implicit accounts | Create a named faucet account with the CLI and send NEAR from it |
