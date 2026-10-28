//! StonkMoney split: the pool creator of a StonkFun (Raydium LaunchLab) coin is this program's per-coin
//! inbox PDA. StonkFun forwards the creator's share of the trading fee to that address as plain SOL;
//! `distribute` books it into the coin's slots (wallets and X / Telegram / GitHub handles) and the
//! platform share; owners claim. Handles are bound to a wallet by our server key (bounded: a daily cap
//! on what handle claims may pay out). See README.md.
#![cfg_attr(not(test), no_std)]

use pinocchio::cpi::{Seed, Signer};
use pinocchio::error::ProgramError;
use pinocchio::sysvars::{clock::Clock, rent::Rent, Sysvar};
use pinocchio::{AccountView, Address, ProgramResult};
use pinocchio_system::instructions::{Allocate, Assign, Transfer};

#[cfg(not(feature = "no-entrypoint"))]
pinocchio::program_entrypoint!(process_instruction, 24);
pinocchio::no_allocator!();
pinocchio::nostd_panic_handler!();

// Program address.
pub const ID: Address = Address::from_str_const("HxNq5HC7Sz6mwYTGv4wQeH7LVWFNYVMrK9XE55hvfGZk");
const SYSTEM: Address = pinocchio_system::ID;
const LOADER: Address = Address::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
const SYSVAR: Address = Address::from_str_const("Sysvar1111111111111111111111111111111111111");
const ZERO: Address = Address::new_from_array([0; 32]);
// Runtime-reserved ids that are never writable yet are neither executable nor sysvar-owned.
const RESERVED: [Address; 5] = [
    Address::from_str_const("StakeConfig11111111111111111111111111111111"),
    Address::from_str_const("LoaderV411111111111111111111111111111111111"),
    Address::from_str_const("ZkTokenProof1111111111111111111111111111111"),
    Address::from_str_const("NativeLoader1111111111111111111111111111111"),
    SYSVAR,
];

const BPS: u64 = 10_000;
const MAX_SLOTS: usize = 10;
const MAX_PLAT_BPS: u16 = 5_000;
const MAX_TIP: u64 = 50_000;
const DAY: i64 = 86_400;
// defaults set by init_config, changed by set_config
const PLAT_BPS: u16 = 2_000; // TZ: 1 of the 5 %, i.e. a fifth of the creator's fee stream
const CAP: u64 = 5_000_000_000; // handle claims may pay out at most 5 SOL a day
const TIP: u64 = 10_000; // to whoever pays the fee of a distribute / sponsored claim

const CONFIG: u8 = 1;
const COIN: u8 = 2;
// slot kinds: a plain wallet (owner fixed at registration) or a social handle (owner bound by our server)
const WALLET: u8 = 0;
const MAX_KIND: u8 = 3; // 1 X, 2 Telegram, 3 GitHub
const BOUND: u8 = 1; // slot flag: owner set by the binder, claims count toward the daily cap

mod c {
    pub const BUMP: usize = 1;
    pub const ADMIN: usize = 2;
    pub const PLATFORM: usize = 34;
    pub const BINDER: usize = 66;
    pub const COIN_MINT: usize = 98; // $STONKMONEY, set once by the admin on coin day (unused by v1 logic)
    pub const PLAT_BPS: usize = 130; // u16, share of every booked forward (snapshot per coin at register)
    pub const PAUSED: usize = 132;
    pub const CAP: usize = 133; // u64, lamports handle claims may pay out per UTC day
    pub const DAY: usize = 141; // i64, day of `USED`
    pub const USED: usize = 149; // u64
    pub const TIP: usize = 157; // u64
    pub const OPEN: usize = 165; // u32, registered coins not yet refunded
    pub const LEN: usize = 169;
}
mod k {
    pub const BUMP: usize = 1;
    pub const IBUMP: usize = 2; // inbox bump
    pub const N: usize = 3;
    pub const PLAT_BPS: usize = 4; // u16
    pub const MINT: usize = 6;
    pub const AUTHOR: usize = 38; // registered the coin, paid its rent; gets unclaimed handle money on wind-down
    pub const TOTAL: usize = 70; // u64, lifetime booked (stats)
    pub const PLAT_DUE: usize = 78; // u64
    pub const SLOTS: usize = 86;
    pub const LEN: usize = SLOTS + super::MAX_SLOTS * super::s::LEN;
}
mod s {
    pub const KIND: usize = 0;
    pub const FLAGS: usize = 1;
    pub const KEY: usize = 2; // wallet address, or a 32-byte handle id chosen by the site
    pub const BPS: usize = 34; // u16, share of the after-platform rest
    pub const OWNER: usize = 36; // zero until bound
    pub const DUE: usize = 68; // u64
    pub const LEN: usize = 76;
}

#[repr(u32)]
enum Error {
    Unauthorized = 1,
    Paused,
    NotPaused,
    BadParams,
    BadAccount,
    MathOverflow,
    AlreadyBound,
    NothingToClaim,
    CapReached,
    PayoutBelowRent,
    CoinsOpen,
}
fn err(e: Error) -> ProgramError {
    ProgramError::Custom(e as u32)
}
fn need(ok: bool, e: Error) -> ProgramResult {
    if ok {
        Ok(())
    } else {
        Err(err(e))
    }
}
fn rd<const N: usize>(d: &[u8], o: usize) -> Result<[u8; N], ProgramError> {
    let end = o.checked_add(N).ok_or(ProgramError::InvalidAccountData)?;
    d.get(o..end)
        .ok_or(ProgramError::InvalidAccountData)?
        .try_into()
        .map_err(|_| ProgramError::InvalidAccountData)
}
fn byte(d: &[u8], o: usize) -> Result<u8, ProgramError> {
    d.get(o).copied().ok_or(ProgramError::InvalidAccountData)
}
fn key(d: &[u8], o: usize) -> Result<Address, ProgramError> {
    Ok(Address::new_from_array(rd(d, o)?))
}
fn u16_at(d: &[u8], o: usize) -> Result<u16, ProgramError> {
    Ok(u16::from_le_bytes(rd(d, o)?))
}
fn u32_at(d: &[u8], o: usize) -> Result<u32, ProgramError> {
    Ok(u32::from_le_bytes(rd(d, o)?))
}
fn u64_at(d: &[u8], o: usize) -> Result<u64, ProgramError> {
    Ok(u64::from_le_bytes(rd(d, o)?))
}
fn put(d: &mut [u8], o: usize, v: &[u8]) -> ProgramResult {
    let end = o.checked_add(v.len()).ok_or(ProgramError::InvalidAccountData)?;
    d.get_mut(o..end)
        .ok_or(ProgramError::InvalidAccountData)?
        .copy_from_slice(v);
    Ok(())
}
fn add(a: u64, b: u64) -> Result<u64, ProgramError> {
    a.checked_add(b).ok_or(err(Error::MathOverflow))
}
fn sub(a: u64, b: u64) -> Result<u64, ProgramError> {
    a.checked_sub(b).ok_or(err(Error::MathOverflow))
}
fn inc(d: &mut [u8], o: usize, v: u64) -> ProgramResult {
    let x = add(u64_at(d, o)?, v)?;
    put(d, o, &x.to_le_bytes())
}
/// a * b / BPS, b <= BPS
fn part(a: u64, b: u64) -> u64 {
    ((a as u128 * b as u128) / BPS as u128) as u64
}
fn writable(a: &AccountView) -> ProgramResult {
    need(a.is_writable(), Error::BadAccount)
}
fn signer(a: &AccountView) -> ProgramResult {
    if a.is_signer() {
        Ok(())
    } else {
        Err(ProgramError::MissingRequiredSignature)
    }
}
fn system(a: &AccountView) -> ProgramResult {
    need(a.address() == &SYSTEM, Error::BadAccount)
}
fn find(seeds: &[&[u8]], a: &AccountView) -> Result<u8, ProgramError> {
    let (address, bump) = Address::find_program_address(seeds, &ID);
    need(a.address() == &address, Error::BadAccount)?;
    Ok(bump)
}
fn check_pda(a: &AccountView, seeds: &[&[u8]]) -> ProgramResult {
    let address = Address::create_program_address(seeds, &ID).map_err(|_| err(Error::BadAccount))?;
    need(a.address() == &address, Error::BadAccount)
}
fn load<const N: usize>(a: &AccountView, disc: u8) -> Result<[u8; N], ProgramError> {
    if !a.owned_by(&ID) {
        return Err(ProgramError::IllegalOwner);
    }
    if a.data_len() != N {
        return Err(ProgramError::InvalidAccountData);
    }
    let d: [u8; N] = rd(&a.try_borrow()?, 0)?;
    need(d[0] == disc, Error::BadAccount)?;
    Ok(d)
}
fn config(a: &AccountView) -> Result<[u8; c::LEN], ProgramError> {
    let d = load(a, CONFIG)?;
    check_pda(a, &[b"config", &[d[c::BUMP]]])?;
    Ok(d)
}
fn coin(a: &AccountView) -> Result<[u8; k::LEN], ProgramError> {
    let d = load(a, COIN)?;
    check_pda(a, &[b"coin", &d[k::MINT..k::MINT + 32], &[d[k::BUMP]]])?;
    Ok(d)
}
/// The coin's inbox: a data-less system account at ["inbox", mint] (the LaunchLab pool creator).
fn inbox(a: &AccountView, kd: &[u8]) -> ProgramResult {
    need(a.owned_by(&SYSTEM) && a.data_len() == 0, Error::BadAccount)?;
    check_pda(a, &[b"inbox", &kd[k::MINT..k::MINT + 32], &[kd[k::IBUMP]]])
}
fn save(a: &mut AccountView, d: &[u8]) -> ProgramResult {
    writable(a)?;
    let mut data = a.try_borrow_mut()?;
    need(data.len() == d.len(), Error::BadAccount)?;
    data.copy_from_slice(d);
    Ok(())
}
fn live(cfg: &[u8]) -> ProgramResult {
    need(cfg[c::PAUSED] == 0, Error::Paused)
}
fn rent(len: usize) -> Result<u64, ProgramError> {
    Rent::get()?.try_minimum_balance(len)
}
fn slot(i: usize) -> usize {
    k::SLOTS + i * s::LEN
}
fn init_account(payer: &AccountView, new: &AccountView, len: usize, seeds: &[Seed]) -> ProgramResult {
    signer(payer)?;
    writable(payer)?;
    writable(new)?;
    // Never overwrite records; a prefunded empty address is still usable (anti-griefing).
    if !new.owned_by(&SYSTEM) || new.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let shortfall = rent(len)?.saturating_sub(new.lamports());
    if shortfall > 0 {
        Transfer { from: payer, to: new, lamports: shortfall }.invoke()?;
    }
    let signers = [Signer::from(seeds)];
    Allocate { account: new, space: len as u64 }.invoke_signed(&signers)?;
    Assign { account: new, owner: &ID }.invoke_signed(&signers)
}
/// Move lamports out of an account this program owns.
fn pay(from: &mut AccountView, to: &mut AccountView, amount: u64) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    writable(from)?;
    writable(to)?;
    need(from.address() != to.address(), Error::BadAccount)?;
    let left = sub(from.lamports(), amount)?;
    let got = add(to.lamports(), amount)?;
    from.set_lamports(left);
    to.set_lamports(got);
    Ok(())
}
/// An address the runtime can never credit: executable, sysvar-owned, reserved id, or one this
/// instruction cannot write.
fn uncreditable(a: &AccountView) -> bool {
    !a.is_writable() || a.executable() || a.owned_by(&SYSVAR) || RESERVED.contains(a.address())
}
/// Can `to` take `amount`? The runtime refuses a credit that leaves an account below the rent-exempt
/// minimum for its size.
fn takes(to: &AccountView, amount: u64) -> Result<bool, ProgramError> {
    Ok(!uncreditable(to) && (amount == 0 || add(to.lamports(), amount)? >= rent(to.data_len())?))
}
/// A slot owner may not be zero, this program, the system program, or our own config / coin / inbox
/// (paying one of those to itself or back into the inbox would strand money).
fn payee_ok(x: &Address, cfg: &Address, coin_acc: &Address, inb: &Address) -> bool {
    *x != ZERO && x != &ID && *x != SYSTEM && x != cfg && x != coin_acc && x != inb
}
fn inbox_signer<'a>(kd: &'a [u8], ibump: &'a [u8; 1]) -> [Seed<'a>; 3] {
    [Seed::from(b"inbox"), Seed::from(&kd[k::MINT..k::MINT + 32]), Seed::from(ibump)]
}
/// Book `amount` (already in the coin account): platform share first, the rest by slot bps; the
/// last slot takes the rounding dust so the dues always add up to what came in.
fn book(kd: &mut [u8; k::LEN], amount: u64) -> ProgramResult {
    let plat = part(amount, u16_at(kd, k::PLAT_BPS)? as u64);
    inc(kd, k::PLAT_DUE, plat)?;
    let rest = sub(amount, plat)?;
    let n = kd[k::N] as usize;
    let mut given = 0u64;
    for i in 0..n {
        let o = slot(i);
        let share = if i + 1 == n { sub(rest, given)? } else { part(rest, u16_at(kd, o + s::BPS)? as u64) };
        given = add(given, share)?;
        inc(kd, o + s::DUE, share)?;
    }
    inc(kd, k::TOTAL, amount)
}
/// Move everything on the inbox above its rent reserve into the coin account; returns the amount.
fn pull(inb: &AccountView, coin_acc: &AccountView, kd: &[u8]) -> Result<u64, ProgramError> {
    let amount = inb.lamports().saturating_sub(rent(0)?);
    if amount > 0 {
        let ib = [kd[k::IBUMP]];
        let seeds = inbox_signer(kd, &ib);
        Transfer { from: inb, to: coin_acc, lamports: amount }.invoke_signed(&[Signer::from(&seeds)])?;
    }
    Ok(amount)
}

pub fn process_instruction(pid: &Address, a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    need(pid == &ID, Error::Unauthorized)?;
    let (&tag, args) = d.split_first().ok_or(ProgramError::InvalidInstructionData)?;
    match tag {
        0 => init_config(a),
        1 => set_config(a, args),
        2 => set_coin_mint(a, args),
        3 => register(a, args),
        4 => distribute(a),
        5 => bind(a, args),
        6 => claim(a, args),
        7 => claim_platform(a),
        8 => refund_coin(a),
        9 => close_config(a),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// admin / platform / binder may not be this program, its config, the system program, zero; the
/// platform must be creditable and not our state. The admin signs to accept.
fn roles(admin: &AccountView, platform: &AccountView, binder: &AccountView, cfg: &Address) -> ProgramResult {
    signer(admin)?;
    for x in [admin.address(), platform.address(), binder.address()] {
        need(*x != ZERO && x != &ID && x != cfg && *x != SYSTEM, Error::BadParams)?;
    }
    need(!platform.executable() && !platform.owned_by(&SYSVAR) && !platform.owned_by(&ID) && !RESERVED.contains(platform.address()), Error::BadParams)
}

/// [authority(S,W), config(W), program, programdata, system, platform, admin(S), binder].
/// Only the program's upgrade authority may create the config (no init front-running). Starts paused.
fn init_config(a: &mut [AccountView]) -> ProgramResult {
    let [authority, cfg, program, pd, sys, platform, admin, binder, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    signer(authority)?;
    system(sys)?;
    need(program.address() == &ID && program.owned_by(&LOADER) && program.executable(), Error::Unauthorized)?;
    let pd_key = Address::find_program_address(&[ID.as_ref()], &LOADER).0;
    need(pd.address() == &pd_key && pd.owned_by(&LOADER), Error::Unauthorized)?;
    {
        let data = program.try_borrow()?;
        need(rd::<4>(&data, 0)? == 2u32.to_le_bytes() && key(&data, 4)? == pd_key, Error::Unauthorized)?;
        let data = pd.try_borrow()?;
        need(
            rd::<4>(&data, 0)? == 3u32.to_le_bytes() && byte(&data, 12)? == 1 && key(&data, 13)? == *authority.address(),
            Error::Unauthorized,
        )?;
    }
    let bump = [find(&[b"config"], cfg)?];
    roles(admin, platform, binder, cfg.address())?;
    init_account(authority, cfg, c::LEN, &[Seed::from(b"config"), Seed::from(&bump)])?;
    let mut cd = [0u8; c::LEN];
    cd[0] = CONFIG;
    cd[c::BUMP] = bump[0];
    put(&mut cd, c::ADMIN, admin.address().as_ref())?;
    put(&mut cd, c::PLATFORM, platform.address().as_ref())?;
    put(&mut cd, c::BINDER, binder.address().as_ref())?;
    put(&mut cd, c::PLAT_BPS, &PLAT_BPS.to_le_bytes())?;
    cd[c::PAUSED] = 1; // opened by the admin when ready
    put(&mut cd, c::CAP, &CAP.to_le_bytes())?;
    put(&mut cd, c::TIP, &TIP.to_le_bytes())?;
    save(cfg, &cd)
}

/// [admin(S), config(W), platform, new_admin(S) (may be the same key), binder];
/// args: plat_bps u16 (<= 5000), paused u8, cap u64, tip u64 (<= 50 000).
fn set_config(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [admin, cfg, platform, new_admin, binder, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    signer(admin)?;
    need(admin.address() == &key(&cd, c::ADMIN)?, Error::Unauthorized)?;
    let plat_bps = u16_at(d, 0)?;
    let paused = byte(d, 2)?;
    let cap = u64_at(d, 3)?;
    let tip = u64_at(d, 11)?;
    need(plat_bps <= MAX_PLAT_BPS && paused <= 1 && tip <= MAX_TIP, Error::BadParams)?;
    let same = *new_admin.address() == key(&cd, c::ADMIN)?
        && *platform.address() == key(&cd, c::PLATFORM)?
        && *binder.address() == key(&cd, c::BINDER)?;
    if same {
        // unchanged roles are not re-validated, so a pause (the first wind-down step) never stalls on them
        signer(new_admin)?;
    } else {
        roles(new_admin, platform, binder, cfg.address())?;
    }
    put(&mut cd, c::ADMIN, new_admin.address().as_ref())?;
    put(&mut cd, c::PLATFORM, platform.address().as_ref())?;
    put(&mut cd, c::BINDER, binder.address().as_ref())?;
    put(&mut cd, c::PLAT_BPS, &plat_bps.to_le_bytes())?;
    cd[c::PAUSED] = paused;
    put(&mut cd, c::CAP, &cap.to_le_bytes())?;
    put(&mut cd, c::TIP, &tip.to_le_bytes())?;
    save(cfg, &cd)
}

/// [admin(S), config(W)]; args: mint. Once: the $STONKMONEY address is set on coin day.
fn set_coin_mint(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [admin, cfg, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    signer(admin)?;
    need(admin.address() == &key(&cd, c::ADMIN)?, Error::Unauthorized)?;
    need(key(&cd, c::COIN_MINT)? == ZERO, Error::BadParams)?;
    let mint = key(d, 0)?;
    need(mint != ZERO, Error::BadParams)?;
    put(&mut cd, c::COIN_MINT, mint.as_ref())?;
    save(cfg, &cd)
}

/// [author(S,W), mint(S), config(W), coin(W), inbox(W), system];
/// args: n u8 (1..=10), then n x (kind u8, key [32], bps u16), bps adding up to 10 000.
/// Runs in the launch tx before the LaunchLab create that names `inbox` as the pool creator. The mint
/// keypair signs, so only the launcher can register a mint. The inbox gets its rent reserve so a
/// forward of any size lands.
fn register(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [author, mint, cfg, coin_acc, inb, sys, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    live(&cd)?;
    system(sys)?;
    signer(mint)?;
    let n = byte(d, 0)? as usize;
    need((1..=MAX_SLOTS).contains(&n) && d.len() == 1 + n * 35, Error::BadParams)?;
    let m = mint.address().as_ref();
    let bump = [find(&[b"coin", m], coin_acc)?];
    let ibump = [find(&[b"inbox", m], inb)?];
    let mut kd = [0u8; k::LEN];
    kd[0] = COIN;
    kd[k::BUMP] = bump[0];
    kd[k::IBUMP] = ibump[0];
    kd[k::N] = n as u8;
    put(&mut kd, k::PLAT_BPS, &rd::<2>(&cd, c::PLAT_BPS)?)?;
    put(&mut kd, k::MINT, m)?;
    put(&mut kd, k::AUTHOR, author.address().as_ref())?;
    let mut sum = 0u64;
    for i in 0..n {
        let (src, o) = (1 + i * 35, slot(i));
        let kind = byte(d, src)?;
        let id: [u8; 32] = rd(d, src + 1)?;
        let bps = u16_at(d, src + 33)?;
        need(kind <= MAX_KIND && bps > 0 && id != [0; 32], Error::BadParams)?;
        for j in 0..i {
            let p = slot(j);
            need(!(kd[p + s::KIND] == kind && kd[p + s::KEY..p + s::KEY + 32] == id), Error::BadParams)?;
        }
        sum += bps as u64;
        kd[o + s::KIND] = kind;
        put(&mut kd, o + s::KEY, &id)?;
        put(&mut kd, o + s::BPS, &bps.to_le_bytes())?;
        if kind == WALLET {
            let w = Address::new_from_array(id);
            need(payee_ok(&w, cfg.address(), coin_acc.address(), inb.address()), Error::BadParams)?;
            put(&mut kd, o + s::OWNER, &id)?;
        }
    }
    need(sum == BPS && key(&cd, c::PLATFORM)? != *inb.address(), Error::BadParams)?;
    init_account(author, coin_acc, k::LEN, &[Seed::from(b"coin"), Seed::from(m), Seed::from(&bump)])?;
    writable(inb)?;
    let reserve = rent(0)?.saturating_sub(inb.lamports());
    if reserve > 0 {
        Transfer { from: author, to: inb, lamports: reserve }.invoke()?;
    }
    save(coin_acc, &kd)?;
    let open = u32_at(&cd, c::OPEN)?.checked_add(1).ok_or(err(Error::MathOverflow))?;
    put(&mut cd, c::OPEN, &open.to_le_bytes())?;
    save(cfg, &cd)
}

/// Permissionless (our keeper runs it). [config, coin(W), inbox(W), system, caller(W)].
/// Books what StonkFun forwarded to the inbox; the caller gets the tip (min(tip, 1 % of it)) when it
/// can take it. Works while paused.
fn distribute(a: &mut [AccountView]) -> ProgramResult {
    let [cfg, coin_acc, inb, sys, caller, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let cd = config(cfg)?;
    system(sys)?;
    let mut kd = coin(coin_acc)?;
    inbox(inb, &kd)?;
    let amount = pull(inb, coin_acc, &kd)?;
    if amount == 0 {
        return Ok(());
    }
    let tip = u64_at(&cd, c::TIP)?.min(amount / 100);
    let tip = if caller.address() != coin_acc.address() && takes(caller, tip)? { tip } else { 0 };
    pay(coin_acc, caller, tip)?;
    book(&mut kd, sub(amount, tip)?)?;
    save(coin_acc, &kd)
}

/// [binder(S), config, coin(W)]; args: slot u8, owner [32]. Our server, after the person proved the
/// handle through Privy, binds an unbound handle slot to their wallet. Once per slot.
fn bind(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [binder, cfg, coin_acc, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let cd = config(cfg)?;
    live(&cd)?;
    signer(binder)?;
    need(binder.address() == &key(&cd, c::BINDER)?, Error::Unauthorized)?;
    let mut kd = coin(coin_acc)?;
    let i = byte(d, 0)? as usize;
    need(i < kd[k::N] as usize, Error::BadParams)?;
    let o = slot(i);
    need(kd[o + s::KIND] != WALLET, Error::BadParams)?;
    need(key(&kd, o + s::OWNER)? == ZERO, Error::AlreadyBound)?;
    let owner = key(d, 1)?;
    let inb = Address::create_program_address(&[b"inbox", &kd[k::MINT..k::MINT + 32], &[kd[k::IBUMP]]], &ID)
        .map_err(|_| err(Error::BadAccount))?;
    need(payee_ok(&owner, cfg.address(), coin_acc.address(), &inb), Error::BadParams)?;
    put(&mut kd, o + s::OWNER, owner.as_ref())?;
    kd[o + s::FLAGS] |= BOUND;
    save(coin_acc, &kd)
}

/// [owner(S,W), config(W), coin(W), fee_payer(W)]; args: slot u8. Pays the slot's due to its owner.
/// Server-bound slots pay at most what is left of today's cap (the rest stays for later). When another
/// key pays the fee (our server sponsoring an empty wallet) it gets min(tip, due) back.
fn claim(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [owner, cfg, coin_acc, payer, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    signer(owner)?;
    let mut cd = config(cfg)?;
    let mut kd = coin(coin_acc)?;
    let i = byte(d, 0)? as usize;
    need(i < kd[k::N] as usize, Error::BadParams)?;
    let o = slot(i);
    need(key(&kd, o + s::OWNER)? == *owner.address(), Error::Unauthorized)?;
    let mut amount = u64_at(&kd, o + s::DUE)?;
    if kd[o + s::FLAGS] & BOUND != 0 {
        let day = Clock::get()?.unix_timestamp / DAY;
        if i64::from_le_bytes(rd(&cd, c::DAY)?) != day {
            put(&mut cd, c::DAY, &day.to_le_bytes())?;
            put(&mut cd, c::USED, &0u64.to_le_bytes())?;
        }
        let left = u64_at(&cd, c::CAP)?.saturating_sub(u64_at(&cd, c::USED)?);
        amount = amount.min(left);
        need(amount > 0 || u64_at(&kd, o + s::DUE)? == 0, Error::CapReached)?;
        inc(&mut cd, c::USED, amount)?;
        save(cfg, &cd)?;
    }
    need(amount > 0, Error::NothingToClaim)?;
    let tip = if payer.address() != owner.address() && payer.is_signer() { u64_at(&cd, c::TIP)?.min(amount) } else { 0 };
    let tip = if tip > 0 && takes(payer, tip)? { tip } else { 0 };
    let net = sub(amount, tip)?;
    need(takes(owner, net)?, Error::PayoutBelowRent)?;
    pay(coin_acc, payer, tip)?;
    pay(coin_acc, owner, net)?;
    let due = sub(u64_at(&kd, o + s::DUE)?, amount)?;
    put(&mut kd, o + s::DUE, &due.to_le_bytes())?;
    save(coin_acc, &kd)
}

/// Permissionless. [config, coin(W), platform(W)]: pays the coin's platform due to the platform wallet.
fn claim_platform(a: &mut [AccountView]) -> ProgramResult {
    let [cfg, coin_acc, platform, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let cd = config(cfg)?;
    need(platform.address() == &key(&cd, c::PLATFORM)?, Error::Unauthorized)?;
    let mut kd = coin(coin_acc)?;
    let inb = Address::create_program_address(&[b"inbox", &kd[k::MINT..k::MINT + 32], &[kd[k::IBUMP]]], &ID)
        .map_err(|_| err(Error::BadAccount))?;
    need(*platform.address() != inb, Error::BadAccount)?;
    let due = u64_at(&kd, k::PLAT_DUE)?;
    need(due > 0, Error::NothingToClaim)?;
    need(takes(platform, due)?, Error::PayoutBelowRent)?;
    pay(coin_acc, platform, due)?;
    put(&mut kd, k::PLAT_DUE, &0u64.to_le_bytes())?;
    save(coin_acc, &kd)
}

/// Wind-down, while paused. [admin(S), config(W), coin(W), inbox(W), author(W), platform(W), system,
/// then one account per slot in slot order (its owner; the author for an unbound handle)].
/// Books the inbox, pays every due: slot -> owner, unbound handle -> author; an owner that cannot take
/// it -> author, then platform (never blocks). Platform due -> platform. The inbox reserve and the coin
/// rent go back to the author; the coin account closes.
fn refund_coin(a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, coin_acc, inb, author, platform, sys, owners @ ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    signer(admin)?;
    need(admin.address() == &key(&cd, c::ADMIN)?, Error::Unauthorized)?;
    need(cd[c::PAUSED] == 1, Error::NotPaused)?;
    need(platform.address() == &key(&cd, c::PLATFORM)?, Error::Unauthorized)?;
    system(sys)?;
    let mut kd = coin(coin_acc)?;
    inbox(inb, &kd)?;
    need(author.address() == &key(&kd, k::AUTHOR)?, Error::BadAccount)?;
    need(platform.address() != inb.address() && platform.is_writable(), Error::BadAccount)?;
    let n = kd[k::N] as usize;
    need(owners.len() >= n, Error::BadAccount)?;
    let amount = pull(inb, coin_acc, &kd)?;
    book(&mut kd, amount)?;
    // the inbox reserve (paid by the author at register) goes back; the inbox ends empty
    let reserve = inb.lamports();
    if reserve > 0 {
        let ib = [kd[k::IBUMP]];
        let seeds = inbox_signer(&kd, &ib);
        Transfer { from: inb, to: author, lamports: reserve }.invoke_signed(&[Signer::from(&seeds)])?;
    }
    for (i, dest) in owners.iter_mut().take(n).enumerate() {
        let o = slot(i);
        let due = u64_at(&kd, o + s::DUE)?;
        let owner = key(&kd, o + s::OWNER)?;
        let want = if owner == ZERO { *author.address() } else { owner };
        need(dest.address() == &want, Error::BadAccount)?;
        // a plain wallet must come writable: only addresses the runtime never lets write may be read-only
        need(dest.is_writable() || dest.executable() || dest.owned_by(&SYSVAR) || RESERVED.contains(dest.address()), Error::BadAccount)?;
        if payee_ok(dest.address(), cfg.address(), coin_acc.address(), inb.address()) && takes(dest, due)? {
            pay(coin_acc, dest, due)?;
        } else if takes(author, due)? {
            pay(coin_acc, author, due)?;
        } else {
            pay(coin_acc, platform, due)?;
        }
        put(&mut kd, o + s::DUE, &0u64.to_le_bytes())?;
    }
    let plat = u64_at(&kd, k::PLAT_DUE)?;
    if takes(platform, plat)? {
        pay(coin_acc, platform, plat)?;
    } else {
        pay(coin_acc, author, plat)?;
    }
    let rest = coin_acc.lamports();
    pay(coin_acc, author, rest)?;
    coin_acc.try_borrow_mut()?.fill(0);
    coin_acc.close()?;
    let open = u32_at(&cd, c::OPEN)?.saturating_sub(1);
    put(&mut cd, c::OPEN, &open.to_le_bytes())?;
    save(cfg, &cd)
}

/// [admin(S,W), config(W)]: after every coin is refunded, while paused; rent back to the admin.
fn close_config(a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let cd = config(cfg)?;
    signer(admin)?;
    need(admin.address() == &key(&cd, c::ADMIN)?, Error::Unauthorized)?;
    need(cd[c::PAUSED] == 1, Error::NotPaused)?;
    need(u32_at(&cd, c::OPEN)? == 0, Error::CoinsOpen)?;
    let all = cfg.lamports();
    pay(cfg, admin, all)?;
    cfg.try_borrow_mut()?.fill(0);
    cfg.close()
}
