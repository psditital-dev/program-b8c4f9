# stonkmoney-split

Solana program (Pinocchio) that splits a token creator's fee stream between wallets and social handles.

A coin launched on Raydium LaunchLab names this program's per-coin inbox PDA `["inbox", mint]` as its pool creator. The
creator's share of trading fees arrives there as SOL. `distribute` books it into the coin record `["coin", mint]`: a
platform share (basis points fixed at registration) and up to 10 slots (wallets or X / Telegram / GitHub / Discord / TikTok handle ids)
by their basis points. Wallet slots claim directly. A handle slot is bound once to a wallet by the binder key; claims of
binder-bound slots are limited by a daily cap. Paused, the admin refunds every coin (dues to owners, unbound handles to
the coin author) and closes the config.

| tag | instruction |
|---|---|
| 0 | init_config (upgrade authority only; starts paused) |
| 1 | set_config (platform share <= 50 %, pause, daily cap, tip, unclaimed sweep on/off, period, buyback wallet) |
| 2 | set_coin_mint (once) |
| 3 | register (author + mint sign; slots, bps sum 10 000) |
| 4 | distribute (anyone; caller tip) |
| 5 | bind (binder; once per handle slot) |
| 6 | claim (slot owner; optional fee sponsor gets the tip) |
| 7 | claim_platform (anyone) |
| 8 | refund_coin (admin, paused) |
| 9 | close_config (admin, paused, no coins open) |
| 10 | expire (anyone, only when switched on: a slot unclaimed for `period` -> buyback wallet; caller tip) |

Build: `cargo build-sbf`. Client and mainnet-fork tests: `client/` (`node fork.test.mjs`).
