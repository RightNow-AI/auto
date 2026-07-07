//! The daemon entry point: load the contract and the watermark once, then run
//! cycles.
//!
//! `once` runs a single cycle (deterministic for tests, scriptable for e2e
//! legs). Otherwise this polls until an unrecoverable error. The watermark —
//! the last distinct-input count a publish compiled at — is threaded through
//! the loop so a scope recompiles only when its count grows past the last
//! publish; with `config.watermark_path` set it is also loaded at startup and
//! persisted after each publish, so the decision survives a restart (wave-5,
//! ADR-0013 amendment). With `config.supervise`, a *retryable* cycle error is
//! logged and retried after an exponential backoff instead of stopping the
//! daemon; a config-shaped error still stops it loudly (wave 4 default:
//! `supervise = false` — any error stops the daemon, spec/adr/0013).

use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use auto_contract::Contract;

use crate::cycle::run_cycle;
use crate::watermark::{read_watermark, write_watermark};
use crate::{DaemonConfig, DaemonError};

/// Longest a supervised backoff sleep may grow to, milliseconds. Past this the
/// daemon keeps retrying at a fixed one-minute cadence rather than sleeping
/// unboundedly on a persistent (but retryable) fault.
const BACKOFF_CAP_MS: u64 = 60_000;

/// Run the recompile daemon over `config`.
///
/// With `config.once`, runs exactly one cycle (using the loaded watermark) and
/// returns; a publishing cycle still persists the watermark. Otherwise loops
/// forever (see `run_loop`). The loop ends only by returning an error — v0
/// has **no clean-shutdown path**; process kill is the stop. In wave 4 *any*
/// cycle error ends it; under `supervise` a retryable error is retried with
/// backoff and only a config-shaped error ends it (ADR-0013 amendment).
pub fn daemon(config: DaemonConfig) -> Result<(), DaemonError> {
    let contract = load_contract(&config.contract)?;
    // Loud on a corrupt watermark: a file we cannot trust must stop the daemon,
    // never be read as "fresh" and silently skip recompiles.
    let last_compiled = read_watermark(config.watermark_path.as_deref())?;

    if config.once {
        let outcome = run_cycle(&config, &contract, last_compiled)?;
        if outcome.published.is_some() {
            write_watermark(config.watermark_path.as_deref(), outcome.count)?;
        }
        return Ok(());
    }

    // None = unbounded: the production poll loop.
    run_loop(&config, &contract, last_compiled, None)
}

/// The poll loop, factored out with a `max_cycles` bound so the supervised
/// retry logic is testable without spinning forever. Production calls it with
/// `max_cycles = None` (unbounded); tests pass `Some(n)` to run exactly `n`
/// cycles and return. Not public API — the only public entry stays
/// [`daemon`].
///
/// Each cycle: run it against `last_compiled`; on success advance and persist
/// the watermark if it published, reset the failure streak, then sleep
/// `poll_interval_ms`. On a **retryable** error under `supervise`, log it and
/// sleep a backoff derived from the consecutive-failure count ([`next_delay`]).
/// Any other error (not supervised, or config-shaped) returns immediately.
pub(crate) fn run_loop(
    config: &DaemonConfig,
    contract: &Contract,
    mut last_compiled: Option<usize>,
    max_cycles: Option<usize>,
) -> Result<(), DaemonError> {
    let mut consecutive_failures: u32 = 0;
    let mut cycles: usize = 0;
    loop {
        if max_cycles.is_some_and(|max| cycles >= max) {
            return Ok(());
        }
        cycles += 1;

        match run_cycle(config, contract, last_compiled) {
            Ok(outcome) => {
                if outcome.published.is_some() {
                    last_compiled = Some(outcome.count);
                    // Persist before sleeping: a publish that is not recorded
                    // would be re-done redundantly after a restart (harmless,
                    // but the whole point of the watermark is to avoid it).
                    write_watermark(config.watermark_path.as_deref(), outcome.count)?;
                }
                consecutive_failures = 0;
                maybe_sleep(config.poll_interval_ms);
            }
            // Supervised + retryable: log and back off instead of exiting.
            Err(e) if config.supervise && e.is_retryable() => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let delay = next_delay(
                    consecutive_failures,
                    config.poll_interval_ms,
                    BACKOFF_CAP_MS,
                );
                eprintln!(
                    "auto-daemon: cycle failed (retryable): {e}; supervised retry \
                     #{consecutive_failures} in {delay}ms"
                );
                maybe_sleep(delay);
            }
            // Not supervised, or a config-shaped error even when supervised:
            // fail loud and stop.
            Err(e) => return Err(e),
        }
    }
}

/// Backoff sleep, milliseconds, after `consecutive_failures` consecutive
/// retryable failures: `base · 2^consecutive_failures`, saturating, capped at
/// `cap`.
///
/// `consecutive_failures = 0` is the success/normal case and returns `base`
/// (the ordinary poll interval); the first failure doubles it, the second
/// quadruples it, and so on until `cap`. Saturating throughout: a large
/// failure count or a large `base` cannot overflow — it clamps to `cap`.
pub(crate) fn next_delay(consecutive_failures: u32, base: u64, cap: u64) -> u64 {
    // 2^consecutive_failures, saturating to u64::MAX at shift >= 64.
    let factor = 1u64.checked_shl(consecutive_failures).unwrap_or(u64::MAX);
    base.saturating_mul(factor).min(cap)
}

/// Sleep `ms` milliseconds, skipping the syscall entirely for `0` (tests and
/// the e2e run at `poll_interval_ms = 0`; a zero-duration sleep is a needless
/// yield).
fn maybe_sleep(ms: u64) {
    if ms > 0 {
        sleep(Duration::from_millis(ms));
    }
}

/// Load the operator's contract, mapping any load failure to
/// [`DaemonError::Contract`] with the file path.
fn load_contract(path: &Path) -> Result<Contract, DaemonError> {
    auto_contract::parse::load(path).map_err(|e| DaemonError::Contract {
        contract: path.display().to_string(),
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_contract_file_is_a_contract_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = DaemonConfig {
            store: dir.path().join("store.db"),
            contract: dir.path().join("absent.toml"),
            registry_root: dir.path().join("registry"),
            recompile_argv: vec!["true".to_owned(), "{out}".to_owned()],
            poll_interval_ms: 0,
            once: true,
            watermark_path: None,
            supervise: false,
        };
        match daemon(config) {
            Err(DaemonError::Contract { contract, .. }) => {
                assert!(contract.ends_with("absent.toml"), "path: {contract}");
            }
            other => panic!("expected Contract error, got {other:?}"),
        }
    }

    #[test]
    fn next_delay_doubles_per_failure_and_caps() {
        let base = 1_000;
        let cap = 60_000;
        // 0 consecutive failures is the normal-poll / just-succeeded case.
        assert_eq!(next_delay(0, base, cap), 1_000);
        // then doubling per consecutive failure ...
        assert_eq!(next_delay(1, base, cap), 2_000);
        assert_eq!(next_delay(2, base, cap), 4_000);
        assert_eq!(next_delay(3, base, cap), 8_000);
        assert_eq!(next_delay(4, base, cap), 16_000);
        assert_eq!(next_delay(5, base, cap), 32_000);
        // ... until it saturates at the cap and stays there.
        assert_eq!(next_delay(6, base, cap), 60_000);
        assert_eq!(next_delay(7, base, cap), 60_000);
    }

    #[test]
    fn next_delay_never_overflows() {
        // A huge failure count (shift >= 64) and a huge base both clamp to cap
        // rather than overflowing.
        assert_eq!(next_delay(u32::MAX, 1_000, 60_000), 60_000);
        assert_eq!(next_delay(64, 1_000, 60_000), 60_000);
        assert_eq!(next_delay(1, u64::MAX, 60_000), 60_000);
    }

    #[test]
    fn next_delay_zero_base_stays_zero() {
        // The e2e / test cadence: base 0 means retry immediately, at any
        // failure count.
        assert_eq!(next_delay(0, 0, 60_000), 0);
        assert_eq!(next_delay(3, 0, 60_000), 0);
    }
}
